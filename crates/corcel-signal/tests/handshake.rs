use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use corcel_signal::{ClientMessage, ServerMessage, SignalPayload};

#[tokio::test]
async fn host_and_participant_exchange_a_signal() {
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let relay = corcel_signal::relay::spawn(bind)
        .await
        .expect("relay should start");
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), relay.port);

    let channel = uuid::Uuid::new_v4();

    let mut host = corcel_signal::client::connect(
        addr,
        relay.fingerprint,
        ClientMessage::Host { channel },
    )
    .await
    .expect("host should connect");

    let host_peer = match host.inbound.recv().await.expect("host welcome") {
        ServerMessage::Welcome { your_peer, host } => {
            assert_eq!(host, Some(your_peer), "host should be welcomed as its own host");
            your_peer
        }
        other => panic!("expected Welcome, got {other:?}"),
    };

    let mut participant = corcel_signal::client::connect(
        addr,
        relay.fingerprint,
        ClientMessage::Join { channel },
    )
    .await
    .expect("participant should connect");

    let participant_peer = match participant.inbound.recv().await.expect("participant welcome") {
        ServerMessage::Welcome { your_peer, host } => {
            assert_eq!(host, Some(host_peer));
            your_peer
        }
        other => panic!("expected Welcome, got {other:?}"),
    };

    let joined = host
        .inbound
        .recv()
        .await
        .expect("host should hear about the join");
    assert!(matches!(joined, ServerMessage::PeerJoined { peer } if peer == participant_peer));

    participant
        .outbound
        .send(ClientMessage::Relay {
            to: host_peer,
            payload: SignalPayload::Offer {
                sdp: "v=0...".into(),
            },
        })
        .unwrap();

    let relayed = host
        .inbound
        .recv()
        .await
        .expect("host should receive the offer");
    match relayed {
        ServerMessage::Relay {
            from,
            payload: SignalPayload::Offer { sdp },
        } => {
            assert_eq!(from, participant_peer);
            assert_eq!(sdp, "v=0...");
        }
        other => panic!("expected relayed Offer, got {other:?}"),
    }
}
