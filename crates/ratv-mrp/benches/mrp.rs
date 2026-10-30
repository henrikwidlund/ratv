//! MRP `SET_STATE` encode, decode and now-playing update.

use std::hint::black_box;

use bytes::BytesMut;
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use ratv_mrp::proto::{
    CommandInfo, ContentItem, ContentItemMetadata, NowPlayingClient, NowPlayingInfo, PlaybackQueue,
    PlayerPath, SetNowPlayingClientMessage, SetStateMessage, SupportedCommands,
};
use ratv_mrp::{NowPlayingState, PlaybackState, encode, frames};

fn path() -> PlayerPath {
    PlayerPath {
        client: Some(NowPlayingClient {
            bundle_identifier: Some("com.apple.TVMusic".into()),
            display_name: Some("Music".into()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// `SET_STATE` as sent when a song starts.
fn set_state() -> SetStateMessage {
    SetStateMessage {
        player_path: Some(path()),
        playback_state: Some(PlaybackState::Playing as i32),
        playback_state_timestamp: Some(100.0),
        now_playing_info: Some(NowPlayingInfo {
            artist: Some("Artist".into()),
            elapsed_time: Some(10.0),
            timestamp: Some(1000.0),
            playback_rate: Some(1.0),
            ..Default::default()
        }),
        supported_commands: Some(SupportedCommands {
            supported_commands: (0..30)
                .map(|i| CommandInfo {
                    command: Some(i),
                    enabled: Some(true),
                    ..Default::default()
                })
                .collect(),
        }),
        playback_queue: Some(PlaybackQueue {
            location: Some(0),
            content_items: vec![ContentItem {
                identifier: Some("item".into()),
                metadata: Some(ContentItemMetadata {
                    title: Some("Song".into()),
                    album_name: Some("Album".into()),
                    duration: Some(240.0),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn mrp(c: &mut Criterion) {
    let msg = set_state();
    let mut out = BytesMut::new();
    encode(&msg, None, &mut out);
    let bytes = out.clone().freeze();

    let mut g = c.benchmark_group("mrp");
    g.throughput(Throughput::Bytes(bytes.len() as u64));
    g.bench_function("encode_set_state", |b| {
        b.iter(|| {
            out.clear();
            encode(black_box(&msg), None, &mut out);
        });
    });
    g.bench_function("decode_set_state", |b| {
        b.iter(|| {
            let env = frames(black_box(bytes.clone())).next().unwrap().unwrap();
            env.payload::<SetStateMessage>().unwrap()
        });
    });

    let mut client = BytesMut::new();
    encode(
        &SetNowPlayingClientMessage {
            client: path().client,
        },
        None,
        &mut client,
    );
    let client = frames(client.freeze()).next().unwrap().unwrap();
    g.bench_function("apply_set_state", |b| {
        b.iter_batched(
            || {
                let mut s = NowPlayingState::new();
                s.apply(&client).unwrap();
                s
            },
            |mut s| {
                let env = frames(bytes.clone()).next().unwrap().unwrap();
                s.apply(&env).unwrap();
                s.now_playing()
            },
            criterion::BatchSize::SmallInput,
        );
    });
    g.finish();
}

criterion_group!(benches, mrp);
criterion_main!(benches);
