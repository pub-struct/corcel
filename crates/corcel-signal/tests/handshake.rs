use corcel_signal::relay::MEDIA_ALPN;
use corcel_signal::{ClientMessage, EndpointAddr, Reach, ServerMessage};

/// Same-process connections resolve through the local-relay registry
/// (direct loopback addresses), so these tests need no network and no
/// discovery infrastructure.
///
/// One `#[tokio::test]` drives all three scenarios: the client keeps a
/// single process-wide outbound endpoint (matching the app, which has one
/// runtime for life), so separate per-test runtimes would tear down the
/// endpoint's driver tasks under each other.
#[tokio::test]
async fn relay_serves_rooms_media_and_local_reach() {
    room_members_exchange_payloads().await;
    media_connections_are_handed_to_the_caller().await;
    local_network_relay_serves_rooms_without_public_infrastructure().await;
}

async fn room_members_exchange_payloads() {
    let identity = corcel_signal::RelayIdentity::generate().expect("identity should generate");
    let relay = corcel_signal::relay::spawn(&identity, Reach::Global)
        .await
        .expect("relay should start");
    let relay_addr = EndpointAddr::from(relay.endpoint_id);

    let room = uuid::Uuid::new_v4();

    let mut first =
        corcel_signal::client::connect(relay_addr.clone(), ClientMessage::Room { channel: room })
            .await
            .expect("first member should connect");
    let first_peer = match first.inbound.recv().await.expect("first welcome") {
        ServerMessage::RoomWelcome { your_peer, peers } => {
            assert!(peers.is_empty(), "first member should find an empty room");
            your_peer
        }
        other => panic!("expected RoomWelcome, got {other:?}"),
    };

    let mut second =
        corcel_signal::client::connect(relay_addr, ClientMessage::Room { channel: room })
            .await
            .expect("second member should connect");
    let second_peer = match second.inbound.recv().await.expect("second welcome") {
        ServerMessage::RoomWelcome { your_peer, peers } => {
            assert_eq!(peers, vec![first_peer], "second member should see the first");
            your_peer
        }
        other => panic!("expected RoomWelcome, got {other:?}"),
    };

    let joined = first.inbound.recv().await.expect("first should hear about the join");
    assert!(matches!(joined, ServerMessage::PeerJoined { peer } if peer == second_peer));

    second
        .outbound
        .send(ClientMessage::Publish { payload: serde_json::json!({ "hello": "room" }) })
        .unwrap();
    match first.inbound.recv().await.expect("first should receive the broadcast") {
        ServerMessage::Published { from, payload } => {
            assert_eq!(from, second_peer);
            assert_eq!(payload, serde_json::json!({ "hello": "room" }));
        }
        other => panic!("expected Published, got {other:?}"),
    }

    first
        .outbound
        .send(ClientMessage::Direct {
            to: second_peer,
            payload: serde_json::json!({ "just": "you" }),
        })
        .unwrap();
    match second.inbound.recv().await.expect("second should receive the direct payload") {
        ServerMessage::Direct { from, payload } => {
            assert_eq!(from, first_peer);
            assert_eq!(payload, serde_json::json!({ "just": "you" }));
        }
        other => panic!("expected Direct, got {other:?}"),
    }
}

async fn media_connections_are_handed_to_the_caller() {
    let identity = corcel_signal::RelayIdentity::generate().expect("identity should generate");
    let mut relay = corcel_signal::relay::spawn(&identity, Reach::Global)
        .await
        .expect("relay should start");

    let conn = corcel_signal::client::dial(EndpointAddr::from(relay.endpoint_id), MEDIA_ALPN)
        .await
        .expect("media dial should connect");

    let accepted = relay.media.recv().await.expect("relay should hand the media connection over");
    assert_eq!(accepted.alpn(), MEDIA_ALPN);
    drop(conn);
}

async fn local_network_relay_serves_rooms_without_public_infrastructure() {
    let identity = corcel_signal::RelayIdentity::generate().expect("identity should generate");
    let relay = corcel_signal::relay::spawn(&identity, Reach::LocalNetwork)
        .await
        .expect("local relay should start");

    // A local-network link is only as good as the direct addresses it can
    // carry — the endpoint must report some, and no relay addresses at all
    // (nothing public to lean on, by construction).
    assert!(relay.addr.ip_addrs().next().is_some(), "local relay should expose direct addrs");
    assert_eq!(relay.addr.relay_urls().count(), 0, "local relay must not touch a public relay");

    let mut conn = corcel_signal::client::connect(
        EndpointAddr::from(relay.endpoint_id),
        ClientMessage::Room { channel: uuid::Uuid::new_v4() },
    )
    .await
    .expect("room connect should work against a local-network relay");
    assert!(matches!(
        conn.inbound.recv().await.expect("welcome"),
        ServerMessage::RoomWelcome { .. }
    ));
}
