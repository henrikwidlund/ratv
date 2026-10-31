use std::net::SocketAddr;

use ratv_companion::Connection;
use ratv_companion::pairing::{ClientInfo, Flow};
use ratv_hap::Credentials;
use ratv_hap::pair_setup::{self, ClientIdentity, NeedPin};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tracing::{Instrument, Span};

use super::io::pairing_step;
use crate::{Error, PairOptions, log, rng, tcp};

/// The device shows a PIN. Call [`PinPrompt::finish`] with it.
pub struct PinPrompt<S> {
    span: Span,
    stream: S,
    conn: Connection,
    state: NeedPin,
    options: PairOptions,
    client_id: String,
}

impl<S> core::fmt::Debug for PinPrompt<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PinPrompt")
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

/// Connects to `addr` and starts pairing. The device shows a PIN when this returns.
pub async fn pair(addr: SocketAddr, options: &PairOptions) -> Result<PinPrompt<TcpStream>, Error> {
    let span = log::session("companion pairing", Some(addr));
    async {
        let stream = tcp::connect(addr, options.timeout).await?;
        start(stream, options, Span::current()).await
    }
    .instrument(span)
    .await
}

/// Starts pairing on an existing stream.
pub async fn pair_stream<S>(stream: S, options: &PairOptions) -> Result<PinPrompt<S>, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let span = log::session("companion pairing", None);
    start(stream, options, span.clone()).instrument(span).await
}

async fn start<S>(mut stream: S, options: &PairOptions, span: Span) -> Result<PinPrompt<S>, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut conn = Connection::new();
    let mut buf = Vec::with_capacity(512);
    let (m1, state) = pair_setup::start(false);
    let m2 = tokio::time::timeout(
        options.timeout,
        pairing_step(&mut stream, &mut conn, Flow::Setup, true, &m1, &mut buf),
    )
    .await??;
    let state = state.handle_m2(&m2)?;
    tracing::debug!("waiting for PIN");
    Ok(PinPrompt {
        span,
        stream,
        conn,
        state,
        client_id: options.client_id.clone().unwrap_or_else(rng::uuid),
        options: options.clone(),
    })
}

impl<S: AsyncRead + AsyncWrite + Unpin> PinPrompt<S> {
    /// Completes pairing with the PIN shown on the device.
    pub async fn finish(self, pin: &str) -> Result<Credentials, Error> {
        let span = self.span.clone();
        self.finish_inner(pin).instrument(span).await
    }

    async fn finish_inner(mut self, pin: &str) -> Result<Credentials, Error> {
        let mut rng = rng::os();
        let mut buf = Vec::with_capacity(1024);
        let (m3, state) = self.state.submit_pin(pin, &mut rng)?;
        let m4 = tokio::time::timeout(
            self.options.timeout,
            pairing_step(
                &mut self.stream,
                &mut self.conn,
                Flow::Setup,
                false,
                &m3,
                &mut buf,
            ),
        )
        .await??;
        let verified = state.handle_m4(&m4)?;
        tracing::debug!("PIN accepted");

        let metadata = ClientInfo {
            account_id: &self.client_id,
            name: &self.options.name,
            model: &self.options.model,
            mac: rng::bytes(),
            alt_irk: rng::bytes(),
        }
        .encode();
        let identity = ClientIdentity {
            id: self.client_id.as_bytes(),
            metadata: Some(&metadata),
        };
        let (m5, state) = verified.exchange(identity, &mut rng);
        let m6 = tokio::time::timeout(
            self.options.timeout,
            pairing_step(
                &mut self.stream,
                &mut self.conn,
                Flow::Setup,
                false,
                &m5,
                &mut buf,
            ),
        )
        .await??;
        let credentials = state.handle_m6(&m6)?;
        tracing::debug!("paired");
        Ok(credentials)
    }
}
