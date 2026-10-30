//! Companion frames and the messages inside them.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ratv_companion::message::{self, Event, Message};
use ratv_companion::{Connection, pairing, text_input};

fuzz_target!(|data: &[u8]| {
    let mut conn = Connection::with_max_frame_len(1 << 16);
    conn.receive(data);
    while let Ok(Some(frame)) = conn.poll_frame() {
        let _ = pairing::decode(&frame.payload);
        let Ok(msg) = Message::decode(&frame.payload) else {
            continue;
        };
        let _ = (msg.id(), msg.kind(), msg.xid(), msg.error());
        let _ = Event::parse(&msg);
        let _ = text_input::session(&msg);
        let _ = message::session_start_response(&msg);
        let _ = message::volume_response(&msg);
        let _ = message::apps_response(&msg);
        let _ = message::attention_response(&msg);
    }
});
