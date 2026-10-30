//! Reads Opus packets from a CAF file, as written by
//! `afconvert -f caff -d opus@16000 -c 1 in.wav out.caf`.

/// Opus packets of a CAF file.
pub(crate) fn packets(file: &[u8]) -> Result<Vec<&[u8]>, &'static str> {
    if file.get(..4) != Some(b"caff") {
        return Err("not a CAF file");
    }
    let (mut sizes, mut data) = (None, None);
    let mut pos = 8;
    while let Some(header) = file.get(pos..pos + 12) {
        let size = i64::from_be_bytes(header[4..12].try_into().expect("8 bytes"));
        let body = pos + 12;
        // A size of -1 means the chunk lasts until the end of the file.
        let end = usize::try_from(size).map_or(file.len(), |s| body + s);
        let chunk = file.get(body..end).ok_or("short chunk")?;
        match &header[..4] {
            b"pakt" => sizes = Some(packet_sizes(chunk)?),
            // The audio data starts after a 4 byte edit count.
            b"data" => data = Some(chunk.get(4..).ok_or("short data chunk")?),
            _ => {}
        }
        pos = end;
    }
    let (sizes, data) = (sizes.ok_or("no packet table")?, data.ok_or("no data")?);
    let mut packets = Vec::with_capacity(sizes.len());
    let mut offset = 0;
    for size in sizes {
        packets.push(
            data.get(offset..offset + size)
                .ok_or("packet out of range")?,
        );
        offset += size;
    }
    Ok(packets)
}

/// Packet sizes from a CAF packet table. Each size is a big endian base 128 number.
fn packet_sizes(table: &[u8]) -> Result<Vec<usize>, &'static str> {
    let count = table.get(..8).ok_or("short packet table")?;
    let count = i64::from_be_bytes(count.try_into().expect("8 bytes"));
    let count = usize::try_from(count).map_err(|_| "bad packet count")?;
    let mut sizes = Vec::with_capacity(count);
    // After packet count, valid frames, priming frames and remainder frames.
    let mut bytes = table.get(24..).ok_or("short packet table")?.iter();
    for _ in 0..count {
        let mut size = 0;
        loop {
            let b = *bytes.next().ok_or("short packet table")?;
            size = (size << 7) | usize::from(b & 0x7f);
            if b & 0x80 == 0 {
                break;
            }
        }
        sizes.push(size);
    }
    Ok(sizes)
}
