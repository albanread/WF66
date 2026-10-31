//! A test driver off the box (Linux x86-64, no host hooks): boot a built-in
//! session with the prelude and run each file given, printing its output.
//!     wf66-run file.fth ...
use wf64::Wf64Session;

fn main() {
    let mut s = Wf64Session::builtin().expect("boot");
    if let Err((l, e)) = s.eval_source(wf66_box::PRELUDE) {
        panic!("prelude line {l}: {e:#}");
    }
    for f in std::env::args().skip(1) {
        let src = std::fs::read_to_string(&f).expect("read");
        let t = std::time::Instant::now();
        match s.eval_source(&src) {
            Ok(out) => print!("{out}"),
            Err((l, e)) => println!("{f}:{l}: error: {e:#}"),
        }
        println!("[{f}: {} ms]", t.elapsed().as_millis());
    }
}
