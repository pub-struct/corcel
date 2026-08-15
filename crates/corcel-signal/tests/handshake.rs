use corcel_signal::{ClientMessage, ServerMessage, SignalPayload};

#[tokio::test]
async fn host_and_participant_exchange_a_signal() {
    let identity =
        corcel_signal::RelayIdentity::generate().expect("identity should generate");
    let relay = corcel_signal::relay::spawn(&identity)
        .await
        .expect("relay should start");
    // Same-process connections resolve through the local-relay registry
    // (direct loopback addresses), so this test needs no network and no
    // discovery infrastructure.
    let relay_id = relay.endpoint_id;

    let channel = uuid::Uuid::new_v4();

    let mut host = corcel_signal::client::connect(relay_id, ClientMessage::Host { channel })
        .await
        .expect("host should connect");

    let host_peer = match host.inbound.recv().await.expect("host welcome") {
        ServerMessage::Welcome { your_peer, host } => {
            assert_eq!(host, Some(your_peer), "host should be welcomed as its own host");
            your_peer
        }
        other => panic!("expected Welcome, got {other:?}"),
    };

    let mut participant = corcel_signal::client::connect(relay_id, ClientMessage::Join { channel })
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
