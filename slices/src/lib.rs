//! Body-only subprocess contract. No request, HTTP, session, or persistence APIs.

use std::io::{self, Read, Write};

/// The entire stdin payload, followed by EOF. Future binaries must validate it.
pub const INVOCATION: &[u8] = b"ESHOP-BODY/1\n";
/// Maximum response body size accepted by the native adapter.
pub const BODY_LIMIT: usize = 65_536;

/// Validate the fixed invocation without allocating from untrusted input.
pub fn read_invocation(mut input: impl Read) -> io::Result<()> {
    let mut payload = [0; INVOCATION.len()];
    input.read_exact(&mut payload)?;
    let mut extra = [0];
    if payload != INVOCATION || input.read(&mut extra)? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid body invocation",
        ));
    }
    Ok(())
}

/// Write a complete bounded body. Domain code supplies bytes, not HTTP metadata.
pub fn write_body(mut output: impl Write, body: &[u8]) -> io::Result<()> {
    if body.len() > BODY_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "body exceeds boundary limit",
        ));
    }
    output.write_all(body)?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_only_exact_invocation() {
        assert!(read_invocation(INVOCATION).is_ok());
        for invalid in [b"".as_slice(), b"ESHOP-BODY/2\n", b"ESHOP-BODY/1\nx"] {
            assert!(read_invocation(invalid).is_err());
        }
    }

    #[test]
    fn accepts_empty_and_limit_but_never_writes_oversize() {
        let mut output = Vec::new();
        write_body(&mut output, b"").unwrap();
        write_body(&mut output, &vec![b'x'; BODY_LIMIT]).unwrap();
        assert_eq!(output.len(), BODY_LIMIT);
        assert!(write_body(&mut output, &vec![b'x'; BODY_LIMIT + 1]).is_err());
        assert_eq!(output.len(), BODY_LIMIT);
    }

    #[test]
    fn propagates_io_failures() {
        assert!(read_invocation(io::repeat(0)).is_err());
        assert!(write_body(io::Cursor::new(&mut [0; 1][..]), b"too long").is_err());
    }
}
