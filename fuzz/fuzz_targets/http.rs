//! HTTP/RTSP parser on a plain-text channel, fed in two parts.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ratv_airplay::Channel;

fuzz_target!(|data: &[u8]| {
    let split = data.first().map_or(0, |&b| usize::from(b)).min(data.len());
    let mut channel = Channel::new();
    for part in [&data[..split], &data[split..]] {
        channel.receive_buffer(part.len()).extend_from_slice(part);
        loop {
            match channel.poll_message() {
                Ok(Some(m)) => {
                    let _ = (m.status(), m.method(), m.uri(), m.body());
                }
                Ok(None) => break,
                Err(_) => return,
            }
        }
    }
});
