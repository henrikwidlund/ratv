//! Reads the PCM data of a WAV file.

/// Bytes of the `data` chunk. The format is not checked.
pub(crate) fn pcm(file: &[u8]) -> Result<&[u8], &'static str> {
    if file.get(..4) != Some(b"RIFF") || file.get(8..12) != Some(b"WAVE") {
        return Err("not a WAV file");
    }
    let mut pos = 12;
    while let Some(header) = file.get(pos..pos + 8) {
        let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        let body = pos + 8;
        if &header[..4] == b"data" {
            return file.get(body..body + len).ok_or("short data chunk");
        }
        // Chunks are padded to an even length.
        pos = body + len + (len & 1);
    }
    Err("no data chunk")
}
