//! Builds one freecord peer connection: STUN-only config, the H264/Opus-only
//! [`config::media_engine`], and the default NACK/RTCP interceptors, wired to
//! a caller-supplied event handler.

use std::sync::Arc;

use rtc::interceptor::Registry;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use webrtc::peer_connection::{PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler};

use crate::config;

pub async fn build(
    handler: Arc<dyn PeerConnectionEventHandler>,
) -> anyhow::Result<Arc<dyn PeerConnection>> {
    let mut media_engine = config::media_engine()?;
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;

    let pc = PeerConnectionBuilder::new()
        .with_configuration(config::ice_configuration())
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_handler(handler)
        .with_udp_addrs(vec!["0.0.0.0:0"])
        .build()
        .await?;

    Ok(Arc::new(pc))
}
