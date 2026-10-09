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
