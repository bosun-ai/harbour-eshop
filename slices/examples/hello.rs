//! Test-only fixture, never installed by the production binary discovery build.
use eshop_slice_boundary::{read_invocation, write_body, BODY_LIMIT};
use std::io::{self, Write};
use std::time::Duration;

fn main() -> io::Result<()> {
    read_invocation(io::stdin().lock())?;
    match std::env::var("ESHOP_TEST_MODE")
        .unwrap_or_default()
        .as_str()
    {
        "partial" => {
            io::stdout().write_all(b"partial must not escape")?;
            std::process::exit(7);
        }
        "sigkill" | "sigterm" | "sigsegv" => {
            io::stdout().write_all(b"partial must not escape")?;
            io::stdout().flush()?;
            // Linux-only acceptance fixture: terminate this child, without a shell.
            unsafe extern "C" {
                fn signal(signal: std::ffi::c_int, handler: usize) -> usize;
                fn raise(signal: std::ffi::c_int) -> std::ffi::c_int;
            }
            let signal_number = match std::env::var("ESHOP_TEST_MODE").unwrap().as_str() {
                "sigkill" => 9,
                "sigterm" => 15,
                _ => 11,
            };
            // SAFETY: these are Linux signals; zero is SIG_DFL. Reset Rust's SIGSEGV
            // handler so this fixture truly dies by signal instead of panicking.
            unsafe {
                signal(signal_number, 0);
                raise(signal_number);
            }
            panic!("signal fixture unexpectedly survived");
        }
        "hang" => std::thread::sleep(Duration::from_secs(30)),
        "stdout" => io::stdout().write_all(&vec![b'x'; BODY_LIMIT + 1])?,
        "stderr" => io::stderr().write_all(&vec![b'x'; BODY_LIMIT + 1])?,
        "limit" => write_body(io::stdout().lock(), &vec![b'x'; BODY_LIMIT])?,
        "empty" => write_body(io::stdout().lock(), b"")?,
        "diagnostic" => {
            io::stderr().write_all(b"private diagnostic")?;
            write_body(io::stdout().lock(), b"Rust fixture!")?;
        }
        _ => write_body(io::stdout().lock(), b"Rust fixture!")?,
    }
    Ok(())
}
