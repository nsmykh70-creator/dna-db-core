// SPDX-License-Identifier: AGPL-3.0-only
//! Length-prefixed frame codec for the DNA-DB wire protocol.
//! Split from the former server.rs monolith — no behaviour change.

use std::io::{Read, Write};

use crate::protocol::MAX_FRAME_SIZE;

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

pub fn read_frame<R: Read + ?Sized>(stream: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    read_frame_limited(stream, MAX_FRAME_SIZE)
}

pub fn read_frame_limited<R: Read + ?Sized>(
    stream: &mut R,
    maximum: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];

    match stream.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > maximum {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame exceeds limit ({len} > {maximum} bytes)"),
        ));
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    Ok(Some(payload))
}

pub fn write_frame<W: Write + ?Sized>(stream: &mut W, payload: &[u8]) -> std::io::Result<()> {
    stream.write_all(&(payload.len() as u32).to_le_bytes())?;
    stream.write_all(payload)
}

pub fn write_raw<W: Write + ?Sized>(stream: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    stream.write_all(bytes)
}
