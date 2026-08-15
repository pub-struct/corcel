//! The joining side of a wss:// signaling connection.
//!
//! Trust here comes from the invite link, not a CA: the link carries the
//! host's self-signed certificate fingerprint, and this connects only if
//! the presented certificate hashes to exactly that value (TOFU/pinning,
//! the same trust model SSH host keys use).

use std::net::SocketAddr;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::TlsConnector;
use tokio_tungstenite::tungstenite::Message;

use crate::protocol::{ClientMessage, ServerMessage};

pub struct Connection {
    pub outbound: mpsc::UnboundedSender<ClientMessage>,
    pub inbound: mpsc::UnboundedReceiver<ServerMessage>,
}

/// Connects to a host's relay at `addr`, verifying its certificate against
/// `expected_fingerprint` (both come from the invite link), then sends
/// `initial` (a [`ClientMessage::Host`] or [`ClientMessage::Join`]) as the
/// first message. Returns channels for sending further `ClientMessage`s and
/// receiving `ServerMessage`s for the lifetime of the connection.
pub async fn connect(
    addr: SocketAddr,
    expected_fingerprint: [u8; 32],
    initial: ClientMessage,
) -> anyhow::Result<Connection> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(FingerprintVerifier {
            expected: expected_fingerprint,
        }))
        .with_no_client_auth();

    let connector = TlsConnector::from(Arc::new(config));
    let tcp = TcpStream::connect(addr).await?;
    let server_name = ServerName::try_from("freecord-host")?.to_owned();
    let tls = connector.connect(server_name, tcp).await?;

    let url = format!("wss://freecord-host:{}/", addr.port());
    let (ws, _response) = tokio_tungstenite::client_async(url, tls).await?;
    let (mut ws_sink, mut ws_stream) = ws.split();

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ClientMessage>();
    let (in_tx, in_rx) = mpsc::unbounded_channel::<ServerMessage>();

    out_tx.send(initial).ok();

    tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let Ok(text) = serde_json::to_string(&msg) else {
                continue;
            };
            if ws_sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    tokio::spawn(async move {
        while let Some(Ok(Message::Text(text))) = ws_stream.next().await {
            let Ok(server_msg) = serde_json::from_str::<ServerMessage>(&text) else {
                continue;
            };
            if in_tx.send(server_msg).is_err() {
                break;
            }
        }
    });

    Ok(Connection {
        outbound: out_tx,
        inbound: in_rx,
    })
}

#[derive(Debug)]
struct FingerprintVerifier {
    expected: [u8; 32],
}

impl ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let actual: [u8; 32] = Sha256::digest(end_entity.as_ref()).into();
        if actual == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(TlsError::General(
                "server certificate fingerprint does not match the invite link".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
