//! Length-prefixed, kind-tagged framing over a byte stream: `[u32 len][u8 kind]
//! [payload]`, with only the payload counted in `len`. Read complete frames with
//! `read_exact`, including across partial socket reads. Use `kind` to distinguish
//! jzon control frames from raw-byte screen frames, avoiding base64 or number-array
//! encoding for high-frequency pane data.

use std::{
    io::{self, Read, Write},
    time::Duration,
};

/// jzon-encoded control data (a `Command`, or a `Tasks`/`Status` event).
pub const KIND_CONTROL: u8 = 1;
/// A `Screen` event: a jzon header (id, cursor, lines) followed by the raw
/// `contents_formatted` bytes, spliced by [`crate::protocol`].
pub const KIND_SCREEN: u8 = 2;
/// Connection-opening handshake containing the protocol version and launch
/// context. Handshakes are not command frames.
pub const KIND_HELLO: u8 = 3;

/// Maximum frame payload size accepted from readers and emitted by writers. Bound
/// allocations from untrusted length prefixes and check maximum encoded payloads
/// against this limit.
pub const MAX_FRAME: u32 = 64 * 1024 * 1024;

/// Maximum time one frame write to a socket peer may block. A peer that
/// stops draining its socket (crashed, SIGSTOPped, hostile) must not wedge
/// the writer: the daemon's event writes and the client's command writes
/// both cap here and treat expiry as a dead connection.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Write one frame and flush immediately for command/event delivery. Screen emission is
/// already coalesced to one frame per `FRAME_MIN` in `core::run_loop`; flush per
/// emitted frame, not per output byte.
pub fn write_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    // Reject an oversized payload before writing any part of the frame.
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|len| *len <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&[kind])?;
    w.write_all(payload)?;
    w.flush()
}

/// Read one whole frame, blocking until it's complete. Returns `(kind, payload)`.
/// An EOF between frames surfaces as `UnexpectedEof`. The caller treats that as
/// "peer gone".
pub fn read_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut kind = [0u8; 1];
    r.read_exact(&mut kind)?;
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    Ok((kind[0], payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Two frames written back-to-back read back intact and in order, including
    /// an empty payload. The length prefix delimits them, not any separator.
    #[test]
    fn frames_round_trip_back_to_back() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, KIND_CONTROL, b"hello").unwrap();
        write_frame(&mut buf, KIND_SCREEN, b"").unwrap();
        write_frame(&mut buf, KIND_CONTROL, &[0u8, 255, 7, 128]).unwrap();

        let mut cur = Cursor::new(buf);
        assert_eq!(
            read_frame(&mut cur).unwrap(),
            (KIND_CONTROL, b"hello".to_vec())
        );
        assert_eq!(read_frame(&mut cur).unwrap(), (KIND_SCREEN, Vec::new()));
        assert_eq!(
            read_frame(&mut cur).unwrap(),
            (KIND_CONTROL, vec![0, 255, 7, 128])
        );
        // Nothing left → clean EOF.
        assert!(read_frame(&mut cur).is_err());
    }

    /// Reassemble a frame split across header and payload reads through `read_exact`.
    #[test]
    fn split_read_reassembles() {
        let mut whole: Vec<u8> = Vec::new();
        write_frame(&mut whole, KIND_CONTROL, b"split me").unwrap();
        // Feed byte-by-byte through a reader that yields one byte per read.
        struct Trickle<'a>(&'a [u8], usize);
        impl Read for Trickle<'_> {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                if self.1 >= self.0.len() || out.is_empty() {
                    return Ok(0);
                }
                out[0] = self.0[self.1];
                self.1 += 1;
                Ok(1)
            }
        }
        let mut t = Trickle(&whole, 0);
        assert_eq!(
            read_frame(&mut t).unwrap(),
            (KIND_CONTROL, b"split me".to_vec())
        );
    }

    /// Reject an oversized payload before writing a partial frame.
    #[test]
    fn oversized_write_fails_locally() {
        let payload = vec![0u8; MAX_FRAME as usize + 1];
        let mut buf: Vec<u8> = Vec::new();
        let err = write_frame(&mut buf, KIND_CONTROL, &payload).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(buf.is_empty(), "no bytes may reach the stream");
    }
}
