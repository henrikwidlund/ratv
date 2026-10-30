//! Artwork of the current item, from the reply to
//! [`playback_queue_request`](crate::messages::playback_queue_request).

use bytes::Bytes;

use crate::proto::SetStateMessage;
use crate::{Envelope, Error};

/// Image data of the current item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artwork {
    /// Encoded image, shares the buffer of the received message.
    pub data: Bytes,
    /// MIME type, for example `image/jpeg`. Detected from the data if the device does not send it.
    pub mime_type: Option<String>,
    /// Width in pixels.
    pub width: Option<i32>,
    /// Height in pixels.
    pub height: Option<i32>,
}

/// Reads the artwork from a playback queue reply. `None` if the reply has no image.
pub fn artwork(env: &Envelope) -> Result<Option<Artwork>, Error> {
    let msg: SetStateMessage = env.payload()?;
    let Some(queue) = msg.playback_queue else {
        return Ok(None);
    };
    let location = queue
        .location
        .and_then(|l| usize::try_from(l).ok())
        .unwrap_or(0);
    let Some(item) = queue
        .content_items
        .into_iter()
        .nth(location)
        .filter(|i| i.artwork_data.as_ref().is_some_and(|d| !d.is_empty()))
    else {
        return Ok(None);
    };
    let meta = item.metadata.as_ref();
    let data = item.artwork_data.as_deref().unwrap_or_default();
    Ok(Some(Artwork {
        mime_type: meta
            .and_then(|m| m.artwork_mime_type.clone())
            .or_else(|| sniff(data).map(str::to_owned)),
        width: item.artwork_data_width.or_else(|| meta?.artwork_data_width),
        height: item
            .artwork_data_height
            .or_else(|| meta?.artwork_data_height),
        data: item.artwork_data.unwrap_or_default(),
    }))
}

/// MIME type from the first bytes of an image.
const fn sniff(data: &[u8]) -> Option<&'static str> {
    match data {
        [0xff, 0xd8, 0xff, ..] => Some("image/jpeg"),
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some("image/webp"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;

    use super::*;
    use crate::proto::{ContentItem, ContentItemMetadata, PlaybackQueue};
    use crate::{Payload, encode, frames};

    fn env<P: Payload>(p: &P) -> Envelope {
        let mut out = BytesMut::new();
        encode(p, Some("7"), &mut out);
        frames(out.freeze()).next().unwrap().unwrap()
    }

    fn reply(items: Vec<ContentItem>, location: Option<i32>) -> Envelope {
        env(&SetStateMessage {
            playback_queue: Some(PlaybackQueue {
                location,
                content_items: items,
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    #[test]
    fn reads_image() {
        let item = ContentItem {
            artwork_data: Some(Bytes::from_static(b"\xff\xd8jpeg")),
            artwork_data_height: Some(300),
            metadata: Some(ContentItemMetadata {
                artwork_mime_type: Some("image/jpeg".into()),
                artwork_data_width: Some(400),
                ..Default::default()
            }),
            ..Default::default()
        };
        let art = artwork(&reply(vec![ContentItem::default(), item], Some(1)))
            .unwrap()
            .unwrap();
        assert_eq!(&art.data[..], b"\xff\xd8jpeg");
        assert_eq!(art.mime_type.as_deref(), Some("image/jpeg"));
        assert_eq!((art.width, art.height), (Some(400), Some(300)));
    }

    #[test]
    fn detects_mime_type() {
        let item = ContentItem {
            artwork_data: Some(Bytes::from_static(b"\x89PNG\r\n")),
            ..Default::default()
        };
        let art = artwork(&reply(vec![item], None)).unwrap().unwrap();
        assert_eq!(art.mime_type.as_deref(), Some("image/png"));
        assert_eq!(sniff(b"\xff\xd8\xff\xe0"), Some("image/jpeg"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8"), Some("image/webp"));
        assert_eq!(sniff(b"GIF89a"), None);
    }

    #[test]
    fn missing_image() {
        assert_eq!(artwork(&reply(Vec::new(), None)).unwrap(), None);
        let empty = ContentItem {
            artwork_data: Some(Bytes::new()),
            ..Default::default()
        };
        assert_eq!(artwork(&reply(vec![empty], None)).unwrap(), None);
        assert_eq!(artwork(&env(&SetStateMessage::default())).unwrap(), None);
    }
}
