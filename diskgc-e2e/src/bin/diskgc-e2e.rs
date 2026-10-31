//! In-container harness and destructive tests. The interlock runs first;
//! outside a diskgc test container nothing else runs (docs/testing.md,
//! Safety interlock).

use diskgc_e2e::{exit, interlock, probe, suite, under_test};

const USAGE: &str = "\
usage: diskgc-e2e list            list the tests (name<TAB>tier)
       diskgc-e2e run NAME        run one test (as container root)
       diskgc-e2e probe ARGS...   a probe (see src/probe.rs)
       diskgc-e2e as-tester -- PROGRAM ARGS...
                                  exec PROGRAM as tester (uid 1000)
       diskgc-e2e uname-2.6 -- PROGRAM ARGS...
                                  exec PROGRAM with personality UNAME26
                                  (uname reports a 2.6 kernel)
       diskgc-e2e without-cap-sys-ptrace -- PROGRAM ARGS...
                                  exec PROGRAM without CAP_SYS_PTRACE
Runs only inside a diskgc test container; use `just e2e`.";

fn main() {
    interlock::enforce("diskgc-e2e");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.as_slice() {
        [cmd] if cmd == "list" => suite::list(),
        [cmd, name] if cmd == "run" => suite::run(name),
        [cmd, rest @ ..] if cmd == "probe" => probe::main(rest),
        [cmd, sep, rest @ ..] if cmd == "as-tester" && sep == "--" => {
            under_test::exec_as_tester(rest)
        }
        [cmd, sep, rest @ ..] if cmd == "uname-2.6" && sep == "--" => {
            under_test::exec_with_uname26(rest)
        }
        [cmd, sep, rest @ ..] if cmd == "without-cap-sys-ptrace" && sep == "--" => {
            under_test::exec_without_cap_sys_ptrace(rest)
        }
        _ => {
            eprintln!("{USAGE}");
            exit::USAGE
        }
    };
    std::process::exit(code);
}
