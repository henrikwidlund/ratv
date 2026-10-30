//! AirPlay 2 remote control connections: control, events and data stream.

use std::net::SocketAddr;
use std::time::Duration;

use ratv_airplay::Channel;
use ratv_airplay::datastream::DataStream;
use ratv_airplay::http::Message;
use ratv_airplay::keys::{self, Side};
use ratv_airplay::rtsp::{self, ClientInfo, DataStreamSetup, Session};
use ratv_hap::{Credentials, pair_verify};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::{Error, rng};

pub(crate) const READ_CHUNK: usize = 4096;
/// Time to send TEARDOWN. The response is not awaited.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// Identity sent in the events SETUP.
pub(crate) struct Identity<'a> {
    pub(crate) name: &'a str,
    pub(crate) model: &'a str,
    pub(crate) os_version: &'a str,
    pub(crate) os_build: &'a str,
    pub(crate) device_id: &'a str,
}

/// Open AirPlay connections after setup.
pub(crate) struct Connections {
    pub(crate) control: TcpStream,
    pub(crate) channel: Channel,
    pub(crate) session: Session,
    pub(crate) events: TcpStream,
    pub(crate) event_channel: Channel,
    pub(crate) data: TcpStream,
    pub(crate) stream: DataStream,
}

/// Connects, runs pair-verify and sets up the event and data connections.
pub(crate) async fn connect(
    addr: SocketAddr,
    credentials: &Credentials,
    identity: &Identity<'_>,
    timeout: Duration,
) -> Result<Connections, Error> {
    let mut control = TcpStream::connect(addr).await?;
    control.set_nodelay(true)?;
    let local = control.local_addr()?.ip().to_string();
    let mut channel = Channel::new();
    let mut session = Session::new(&local, rng::u32(), rng::u64(), rng::u32());

    let mut rng = rng::os();
    let (m1, state) = pair_verify::start(credentials, &mut rng);
    session.pair_verify(&mut channel, &m1);
    let m2 = ok(tokio::time::timeout(timeout, exchange(&mut control, &mut channel)).await??)?;
    let (m3, state) = state.handle_m2(&m2.body())?;
    session.pair_verify(&mut channel, &m3);
    let m4 = ok(tokio::time::timeout(timeout, exchange(&mut control, &mut channel)).await??)?;
    let secret = state.handle_m4(&m4.body())?;
    channel.enable_encryption(keys::control(&secret, Side::Client));

    // Borrows end with the block, so TEARDOWN can be sent if a step fails.
    let streams = async {
        let session_uuid = rng::uuid();
        session.setup_events(
            &mut channel,
            &ClientInfo {
                name: identity.name,
                model: identity.model,
                os_name: "iPhone OS",
                os_version: identity.os_version,
                os_build_version: identity.os_build,
                source_version: "550.10",
                device_id: identity.device_id,
                mac_address: identity.device_id,
                session_uuid: &session_uuid,
            },
        );
        let resp = tokio::time::timeout(timeout, exchange(&mut control, &mut channel)).await??;
        let event_port = rtsp::event_port(&resp)?;

        let mut events = TcpStream::connect(SocketAddr::new(addr.ip(), event_port)).await?;
        events.set_nodelay(true)?;
        let mut event_channel = Channel::new();
        event_channel.enable_encryption(keys::events(&secret, Side::Client));

        // RECORD and the data SETUP are sent together. Responses come in request order.
        // The device may send event requests before it answers RECORD.
        session.record(&mut channel);
        let seed = rng::u64();
        let (channel_id, client_uuid) = (rng::uuid(), rng::uuid());
        session.setup_data_stream(
            &mut channel,
            &DataStreamSetup {
                channel_id: &channel_id,
                client_uuid: &client_uuid,
                seed,
            },
        );
        let (record, resp) = tokio::time::timeout(timeout, async {
            let record =
                exchange_with_events(&mut control, &mut channel, &mut events, &mut event_channel)
                    .await?;
            let setup =
                exchange_with_events(&mut control, &mut channel, &mut events, &mut event_channel)
                    .await?;
            Ok::<_, Error>((record, setup))
        })
        .await??;
        rtsp::check(&record)?;
        let (data_port, _) = rtsp::data_stream_port(&resp)?;

        let data = TcpStream::connect(SocketAddr::new(addr.ip(), data_port)).await?;
        data.set_nodelay(true)?;
        let stream = DataStream::new(
            keys::data_stream(&secret, seed, Side::Client),
            0x1_0000_0000 | u64::from(rng::u32()),
        );
        Ok::<_, Error>((events, event_channel, data, stream))
    }
    .await;
    let (events, event_channel, data, stream) = match streams {
        Ok(s) => s,
        Err(err) => {
            teardown(&mut control, &mut channel, &mut session).await;
            return Err(err);
        }
    };
    Ok(Connections {
        control,
        channel,
        session,
        events,
        event_channel,
        data,
        stream,
    })
}

fn ok(resp: Message) -> Result<Message, Error> {
    rtsp::check(&resp)?;
    Ok(resp)
}

/// Sends TEARDOWN so the device ends the session. Errors are ignored.
pub(crate) async fn teardown(
    control: &mut TcpStream,
    channel: &mut Channel,
    session: &mut Session,
) {
    session.teardown(channel);
    let _ = tokio::time::timeout(TEARDOWN_TIMEOUT, flush(control, channel)).await;
}

/// Writes all queued bytes of a channel.
pub(crate) async fn flush(stream: &mut TcpStream, channel: &mut Channel) -> Result<(), Error> {
    if channel.has_transmit() {
        stream.write_all(&channel.take_transmit()).await?;
    }
    Ok(())
}

/// Sends queued requests and waits for the next message.
pub(crate) async fn exchange(
    stream: &mut TcpStream,
    channel: &mut Channel,
) -> Result<Message, Error> {
    flush(stream, channel).await?;
    loop {
        if let Some(m) = channel.poll_message()? {
            return Ok(m);
        }
        if stream.read_buf(channel.receive_buffer(READ_CHUNK)).await? == 0 {
            return Err(Error::Disconnected);
        }
    }
}

/// Like [`exchange`], and answers event requests while waiting.
async fn exchange_with_events(
    stream: &mut TcpStream,
    channel: &mut Channel,
    events: &mut TcpStream,
    event_channel: &mut Channel,
) -> Result<Message, Error> {
    flush(stream, channel).await?;
    loop {
        if let Some(m) = channel.poll_message()? {
            return Ok(m);
        }
        tokio::select! {
            n = stream.read_buf(channel.receive_buffer(READ_CHUNK)) => {
                if n? == 0 {
                    return Err(Error::Disconnected);
                }
            }
            n = events.read_buf(event_channel.receive_buffer(READ_CHUNK)) => {
                if n? == 0 {
                    return Err(Error::Disconnected);
                }
                acknowledge_events(events, event_channel).await?;
            }
        }
    }
}

/// Answers all complete event requests with 200.
pub(crate) async fn acknowledge_events(
    events: &mut TcpStream,
    event_channel: &mut Channel,
) -> Result<(), Error> {
    while let Some(req) = event_channel.poll_message()? {
        tracing::trace!(method = ?req.method(), uri = ?req.uri(), "event request");
        event_channel.acknowledge(&req);
    }
    flush(events, event_channel).await
}
