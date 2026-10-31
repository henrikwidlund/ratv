//! Keyboard text over the RTI service.
//!
//! [`TextInputStart`](crate::message::TextInputStart) returns the focused text
//! field as an `NSKeyedArchiver` plist in `_tiD`. Text is sent in a
//! [`TextInputCommand`] event with an archived `RTITextOperations` object.

use std::borrow::Cow;

use ratv_codec::bplist::{self, Value};
use ratv_codec::opack::Encoder;

use crate::Error;
use crate::message::{Message, MessageType, Outgoing};

/// Focused text field.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TextSession {
    /// Session UUID bytes, used as target of text operations.
    pub uuid: Vec<u8>,
    /// Text before the cursor.
    pub text: String,
}

/// Reads the focused text field from a `_tiStart` response.
/// `None` if no text field has focus.
pub fn session(msg: &Message<'_>) -> Result<Option<TextSession>, Error> {
    let Some(data) = msg
        .field("_tiD")
        .and_then(ratv_codec::opack::Value::as_bytes)
    else {
        return Ok(None);
    };
    let archive = bplist::decode(data).map_err(|_| Error::InvalidField("_tiD"))?;
    let uuid = archived(&archive, &["sessionUUID"])
        .and_then(Value::as_bytes)
        .ok_or(Error::InvalidField("sessionUUID"))?;
    let text = archived(&archive, &["documentState", "docSt", "contextBeforeInput"])
        .and_then(Value::as_str)
        .unwrap_or_default();
    Ok(Some(TextSession {
        uuid: uuid.to_vec(),
        text: text.to_owned(),
    }))
}

/// Follows `path` from `$top`, resolving UID references into `$objects`.
fn archived<'v, 'a>(archive: &'v Value<'a>, path: &[&str]) -> Option<&'v Value<'a>> {
    let objects = archive.get("$objects")?.as_array()?;
    let mut element = archive.get("$top")?;
    for key in path {
        element = element.get(key)?;
        if let Value::Uid(i) = element {
            element = objects.get(usize::try_from(*i).ok()?)?;
        }
    }
    Some(element)
}

/// Sends archived text operations (`_tiC`).
#[derive(Debug, Clone, Copy)]
pub struct TextInputCommand<'a> {
    /// Output of [`insert_text`] or [`clear_text`].
    pub data: &'a [u8],
}

impl Outgoing for TextInputCommand<'_> {
    const ID: &'static str = "_tiC";
    const TYPE: MessageType = MessageType::Event;

    fn content(&self, e: &mut Encoder<'_>) {
        e.dict(2, |e| {
            e.str("_tiV").int(1);
            e.str("_tiD").data(self.data);
        });
    }
}

/// Archived operation that inserts `text` at the cursor.
#[must_use]
pub fn insert_text(uuid: &[u8], text: &str) -> Vec<u8> {
    operations(
        uuid,
        vec![("insertionText".into(), Value::Uid(3))],
        Value::from(text),
        None,
    )
}

/// Archived operation that replaces all text with an empty string.
#[must_use]
pub fn clear_text(uuid: &[u8]) -> Vec<u8> {
    operations(uuid, Vec::new(), Value::from(""), Some(3))
}

/// Builds the `RTITextOperations` archive. Object 3 is `string`;
/// `assert_uid` sets `textToAssert` to it.
fn operations(
    uuid: &[u8],
    mut output: Vec<(Cow<'_, str>, Value<'_>)>,
    string: Value<'_>,
    assert_uid: Option<u64>,
) -> Vec<u8> {
    let class = |name: &'static str| {
        Value::Dict(vec![
            ("$classname".into(), Value::from(name)),
            (
                "$classes".into(),
                Value::Array(vec![Value::from(name), Value::from("NSObject")]),
            ),
        ])
    };
    output.push(("$class".into(), Value::Uid(4)));
    let mut ops = vec![
        ("$class".into(), Value::Uid(7)),
        ("targetSessionUUID".into(), Value::Uid(5)),
        ("keyboardOutput".into(), Value::Uid(2)),
    ];
    if let Some(uid) = assert_uid {
        ops.push(("textToAssert".into(), Value::Uid(uid)));
    }
    Value::Dict(vec![
        ("$version".into(), Value::Int(100_000)),
        ("$archiver".into(), Value::from("RTIKeyedArchiver")),
        (
            "$top".into(),
            Value::Dict(vec![("textOperations".into(), Value::Uid(1))]),
        ),
        (
            "$objects".into(),
            Value::Array(vec![
                Value::from("$null"),
                Value::Dict(ops),
                Value::Dict(output),
                string,
                class("TIKeyboardOutput"),
                Value::Dict(vec![
                    ("NS.uuidbytes".into(), Value::Data(Cow::Borrowed(uuid))),
                    ("$class".into(), Value::Uid(6)),
                ]),
                class("NSUUID"),
                class("RTITextOperations"),
            ]),
        ),
    ])
    .encode()
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: [u8; 16] = [7; 16];

    /// Archive shaped like the one tvOS sends in `_tiD`.
    fn start_archive(text: Option<&str>) -> Vec<u8> {
        let mut objects = vec![
            Value::from("$null"),
            Value::Data(Cow::Borrowed(&UUID)),
            Value::Dict(vec![("docSt".into(), Value::Uid(3))]),
        ];
        let mut doc = Vec::new();
        if text.is_some() {
            doc.push(("contextBeforeInput".into(), Value::Uid(4)));
        }
        objects.push(Value::Dict(doc));
        objects.extend(text.map(Value::from));
        Value::Dict(vec![
            (
                "$top".into(),
                Value::Dict(vec![
                    ("sessionUUID".into(), Value::Uid(1)),
                    ("documentState".into(), Value::Uid(2)),
                ]),
            ),
            ("$objects".into(), Value::Array(objects)),
        ])
        .encode()
    }

    fn start_response(archive: Option<&[u8]>) -> Message<'static> {
        let mut out = Vec::new();
        Encoder::new(&mut out).dict(3, |e| {
            e.str("_x").int(1);
            e.str("_t").int(3);
            e.str("_c");
            match archive {
                Some(a) => e.dict(1, |e| {
                    e.str("_tiD").data(a);
                }),
                None => e.dict(0, |_| {}),
            };
        });
        Message::decode(&out).unwrap().into_owned()
    }

    #[test]
    fn reads_session() {
        let a = start_archive(Some("hel"));
        let s = session(&start_response(Some(&a))).unwrap().unwrap();
        assert_eq!(s.uuid, UUID);
        assert_eq!(s.text, "hel");

        let a = start_archive(None);
        let s = session(&start_response(Some(&a))).unwrap().unwrap();
        assert_eq!(s.text, "");

        assert_eq!(session(&start_response(None)).unwrap(), None);
        assert!(session(&start_response(Some(b"not a plist"))).is_err());
    }

    fn operation(data: &[u8], path: &[&str]) -> Option<String> {
        let v = bplist::decode(data).unwrap();
        let mut full = vec!["textOperations"];
        full.extend_from_slice(path);
        archived(&v, &full)
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    #[test]
    fn builds_operations() {
        let insert = insert_text(&UUID, "abc");
        assert_eq!(
            operation(&insert, &["keyboardOutput", "insertionText"]).as_deref(),
            Some("abc")
        );
        assert_eq!(operation(&insert, &["textToAssert"]), None);
        let v = bplist::decode(&insert).unwrap();
        let uuid = archived(&v, &["textOperations", "targetSessionUUID", "NS.uuidbytes"]);
        assert_eq!(uuid.and_then(Value::as_bytes), Some(&UUID[..]));
        assert_eq!(
            archived(&v, &["textOperations", "$class", "$classname"]).and_then(Value::as_str),
            Some("RTITextOperations")
        );

        let clear = clear_text(&UUID);
        assert_eq!(operation(&clear, &["textToAssert"]).as_deref(), Some(""));
        assert_eq!(
            operation(&clear, &["keyboardOutput", "insertionText"]),
            None
        );
    }

    #[test]
    fn command_event() {
        let mut out = Vec::new();
        crate::message::encode(&TextInputCommand { data: b"x" }, 5, &mut out);
        let m = Message::decode(&out).unwrap();
        assert_eq!(m.id(), Some("_tiC"));
        assert_eq!(m.kind(), Some(MessageType::Event));
        assert_eq!(
            m.field("_tiV").and_then(ratv_codec::opack::Value::as_i64),
            Some(1)
        );
    }
}
