# MRP virtual touch (removed)

MRP touch was removed because tvOS 26 does not accept it. Companion touch (`_hidT`) works and is used by `AppleTv::swipe`.
This file keeps the old code so it can be tested on newer tvOS versions.

## Results on Apple TV 4K, tvOS 26.6.1

- `RegisterHidDeviceMessage` always returns
  `RegisterHidDeviceResultMessage { error_code: Some(1), device_identifier: Some(0) }`.
  Tested descriptors (absolute, integrated_display, size):
  `(false, false, 1000)`, `(true, true, 1000)`, `(true, false, 1)`, `(false, false, 0)`.
  The `(true, false, 1000)` descriptor used by the client also failed.
- `SendVirtualTouchEventMessage` without registration does nothing.
  Tested (device id, scale): `(1, 1.0)` (bunatv), `(1, 1000.0)`, `(0, 1.0)`.
- bunatv lists touch as working only over Companion. pyatv has no MRP touch.

## How to test again

1. Add the code below back.
2. Run `cargo run --example touch_probe -- <ip:7000> <airplay credentials>`.
3. Look at the focus on the TV after each line. Each swipe goes right.

## ratv-mrp

`lib.rs`:

```rust
/// Phase of a touch event.
pub type TouchPhase = proto::virtual_touch_phase::Enum;
```

`messages.rs` (imports `RegisterHidDeviceMessage`, `SendVirtualTouchEventMessage`,
`VirtualTouchDeviceDescriptor`, `VirtualTouchEvent` from `crate::proto` and `crate::TouchPhase`):

```rust
/// Registers a virtual touch device with a `width` x `height` touch area.
/// The reply has the device id for [`touch`].
#[must_use]
pub fn register_touch_device(width: f32, height: f32) -> RegisterHidDeviceMessage {
    RegisterHidDeviceMessage {
        device_descriptor: Some(VirtualTouchDeviceDescriptor {
            absolute: Some(true),
            integrated_display: Some(false),
            screen_size_width: Some(width),
            screen_size_height: Some(height),
        }),
    }
}

/// Touch event at `x`, `y` on the touch area of `device_id`.
#[must_use]
pub fn touch(device_id: u64, x: f64, y: f64, phase: TouchPhase) -> SendVirtualTouchEventMessage {
    SendVirtualTouchEventMessage {
        virtual_device_id: Some(device_id),
        event: Some(VirtualTouchEvent {
            x: Some(x),
            y: Some(y),
            phase: Some(phase as i32),
            finger: Some(0),
        }),
    }
}
```

Unit test lines:

```rust
let r = register_touch_device(1000.0, 500.0);
assert_eq!(r.device_descriptor.unwrap().screen_size_height, Some(500.0));
let t = touch(3, 0.25, 0.5, TouchPhase::Moved);
assert_eq!(t.virtual_device_id, Some(3));
assert_eq!(t.event.unwrap().phase, Some(TouchPhase::Moved as i32));
```

## ratv `src/mrp.rs`

Also needs: `TouchPhase` in the `ratv_mrp` re-export, `touch` in `use crate::{...}`,
and `mod touch` in `lib.rs` gated by `cfg(any(feature = "companion", feature = "mrp"))`.

```rust
/// Size of the virtual touch area.
const TOUCH_SIZE: f64 = 1000.0;

// Client field, set to `tokio::sync::OnceCell::new()` in connect:
    /// Virtual touch device id, registered on first touch.
    touch_device: tokio::sync::OnceCell<u64>,

// Client methods:
    /// Sends a touch event. `x` and `y` range from 0.0 to 1.0.
    /// Registers a virtual touch device on first use.
    pub async fn touch(&self, x: f64, y: f64, phase: TouchPhase) -> Result<(), Error> {
        let device = self.touch_device().await?;
        let (x, y) = (x.clamp(0.0, 1.0) * TOUCH_SIZE, y.clamp(0.0, 1.0) * TOUCH_SIZE);
        self.send(&messages::touch(device, x, y, phase)).await
    }

    /// Swipes from `from` to `to` in about `duration`. Coordinates range from 0.0 to 1.0.
    pub async fn swipe(
        &self,
        from: (f64, f64),
        to: (f64, f64),
        duration: Duration,
    ) -> Result<(), Error> {
        touch::swipe(from, to, duration, |x, y, phase| {
            let phase = match phase {
                touch::Phase::Begin => TouchPhase::Began,
                touch::Phase::Move => TouchPhase::Moved,
                touch::Phase::End => TouchPhase::Ended,
            };
            self.touch(x, y, phase)
        })
        .await
    }

    async fn touch_device(&self) -> Result<u64, Error> {
        self.touch_device
            .get_or_try_init(|| async {
                #[allow(clippy::cast_possible_truncation)]
                let size = TOUCH_SIZE as f32;
                let env = self
                    .request(&messages::register_touch_device(size, size))
                    .await?;
                let reply: proto::RegisterHidDeviceResultMessage = env.payload()?;
                match reply.error_code.unwrap_or(0) {
                    0 => Ok(u64::try_from(reply.device_identifier.unwrap_or(0)).unwrap_or(0)),
                    code => Err(ratv_mrp::Error::Device {
                        code,
                        description: "touch device registration failed".into(),
                    }
                    .into()),
                }
            })
            .await
            .copied()
    }
```

`AppleTv::swipe` fallback:

```rust
match &self.companion {
    Some(c) => c.swipe(from, to, duration).await,
    None => self.need_mrp()?.swipe(from, to, duration).await,
}
```

## Fake device in `tests/mrp.rs`

```rust
// DeviceLog field:
    /// Virtual touch events as (device, x, y, phase).
    touches: Vec<(u64, f64, f64, i32)>,

// In handle():
    if env.is::<proto::RegisterHidDeviceMessage>() {
        let r: proto::RegisterHidDeviceMessage = env.payload().unwrap();
        assert_eq!(r.device_descriptor?.screen_size_width, Some(1000.0));
        let result = proto::RegisterHidDeviceResultMessage {
            error_code: Some(0),
            device_identifier: Some(4),
        };
        return Some(mrp(&result, id, 0));
    }
    if env.is::<proto::SendVirtualTouchEventMessage>() {
        let t: proto::SendVirtualTouchEventMessage = env.payload().unwrap();
        let e = t.event?;
        log.lock()
            .unwrap()
            .touches
            .push((t.virtual_device_id?, e.x?, e.y?, e.phase?));
        return None;
    }

// In the session test:
    client
        .swipe((0.5, 0.7), (0.5, 0.3), Duration::from_millis(32))
        .await
        .unwrap();
    // ...
    // Registered device 4; began, one move, ended on a 1000 x 1000 area.
    let touches: Vec<_> = log.touches.iter().map(|t| (t.0, t.2.round(), t.3)).collect();
    assert_eq!(touches, [(4, 700.0, 1), (4, 500.0, 2), (4, 300.0, 4)]);
```

## `examples/mrp.rs` swipe commands

```rust
    if let Some((from, to)) = command.strip_prefix("swipe_").and_then(swipe_points) {
        client.swipe(from, to, Duration::from_millis(200)).await?;
        client.close().await?;
        return Ok(());
    }
```

`swipe_points` is the same helper as in `examples/remote.rs`.

## `examples/touch_probe.rs`

Add to `crates/ratv/Cargo.toml`:

```toml
[[example]]
name = "touch_probe"
required-features = ["mrp"]
```

```rust
//! Tries MRP virtual touch variants. Watch the TV focus after each line.

use std::time::Duration;

use ratv::mrp::{Client, ClientOptions, TouchPhase};
use ratv_mrp::messages::touch;
use ratv_mrp::proto::{RegisterHidDeviceMessage, RegisterHidDeviceResultMessage, VirtualTouchDeviceDescriptor};

async fn swipe(client: &Client, device: u64, scale: f64) -> Result<(), ratv::Error> {
    let steps = 12;
    for i in 0..=steps {
        let x = (0.3 + 0.4 * f64::from(i) / f64::from(steps)) * scale;
        let phase = match i {
            0 => TouchPhase::Began,
            n if n == steps => TouchPhase::Ended,
            _ => TouchPhase::Moved,
        };
        client.send(&touch(device, x, 0.5 * scale, phase)).await?;
        tokio::time::sleep(Duration::from_millis(16)).await;
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let addr = args.first().ok_or("missing address")?.parse()?;
    let credentials = args
        .get(1)
        .ok_or("missing credentials")?
        .parse()
        .map_err(|_| "invalid credentials")?;
    let client = Client::connect(addr, &credentials, &ClientOptions::default()).await?;

    for (absolute, integrated, size) in [
        (false, false, 1000.0),
        (true, true, 1000.0),
        (true, false, 1.0),
        (false, false, 0.0),
    ] {
        let msg = RegisterHidDeviceMessage {
            device_descriptor: Some(VirtualTouchDeviceDescriptor {
                absolute: Some(absolute),
                integrated_display: Some(integrated),
                screen_size_width: Some(size),
                screen_size_height: Some(size),
            }),
        };
        match client.request(&msg).await {
            Ok(env) => {
                let r: RegisterHidDeviceResultMessage = env.payload()?;
                println!("register absolute={absolute} integrated={integrated} size={size}: {r:?}");
                if r.error_code.unwrap_or(0) == 0 {
                    let id = u64::try_from(r.device_identifier.unwrap_or(0)).unwrap_or(0);
                    swipe(&client, id, f64::from(size).max(1.0)).await?;
                    println!("  swiped right with id {id}");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
            Err(err) => println!("register absolute={absolute} integrated={integrated} size={size}: {err}"),
        }
    }
    for (id, scale) in [(1, 1.0), (1, 1000.0), (0, 1.0)] {
        println!("no registration, id {id}, scale {scale}: swiping right");
        swipe(&client, id, scale).await?;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    client.close().await?;
    Ok(())
}
```
