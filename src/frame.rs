//! Length-prefixed, kind-tagged framing over a byte stream: `[u32 len][u8 kind]
//! [payload]`, `len` counting the payload only. `read_frame` uses `read_exact`,
//! so a frame split across partial socket reads reassembles correctly. `kind`
//! separates jzon control frames from the raw-bytes screen frames, so
//! high-frequency pane data pays no base64/number-array tax.

use std::io::{self, Read, Write};

/// jzon-encoded control data (a `Command`, or a `Tasks`/`Status` event).
pub const KIND_CONTROL: u8 = 1;
/// A `Screen` event: a jzon header (id, cursor, lines) followed by the raw
/// `contents_formatted` bytes, spliced by [`crate::protocol`].
pub const KIND_SCREEN: u8 = 2;

/// Reject an absurd length prefix (corrupt or hostile peer) before allocating.
/// 64 MiB is far above any real frame. A full 8K screen's formatted bytes are
/// a few hundred KiB at most.
const MAX_FRAME: u32 = 64 * 1024 * 1024;

/// Write one frame and flush. Flushing per frame keeps latency low: the peer sees
/// each command/event immediately. A firehose can't drown the socket because the
/// core loop already coalesces screen emission to one frame per `FRAME_MIN` (see
/// `core::run_loop`). The flush here is per *emitted* frame, not per output byte.
pub fn write_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
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

    /// A frame arriving in two reads (header, then payload) still reassembles.
    /// `read_exact` is what makes partial socket reads safe.
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
}
