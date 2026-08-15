//! Routes SDP/ICE [`SignalPayload`]s between a [`PeerConnection`] and the
//! peer on the other end of a `freecord-signal` connection.
//!
//! Symmetric by design: the same function drives both directions of a
//! connection. It's used for the participant's initial offer/answer *and*
//! for any later host-initiated renegotiation (e.g. adding a newly forwarded
//! track), since in both cases "receive an Offer, answer it" and "receive an
//! Answer, apply it" are the only two shapes that occur.
//!
//! Both ends can independently call `add_track` (the host to forward a peer's
//! stream, the participant to publish its own mic/screen), so both can
//! independently start renegotiating at the same time — "glare". Without
//! handling it, applying the other side's offer while our own is still
//! outstanding fails outright ("invalid proposed signaling state transition")
//! and leaves the connection's transceivers in an inconsistent state. This
//! follows the standard "perfect negotiation" pattern (see the [MDN glare
//! section]): every connection has one polite and one impolite side; on
//! collision the polite side rolls back its own offer and accepts the
//! incoming one, the impolite side just ignores the incoming one and lets its
//! own offer proceed. Whichever change that discarded offer would have made
//! is not lost — losing/rolling back a local description re-triggers
//! `on_negotiation_needed` once the dust settles, per the JSEP negotiation
//! steps `rtc`'s `PeerConnection` runs after every `set_*_description` call.
//!
//! [MDN glare section]: https://developer.mozilla.org/en-US/docs/Web/API/WebRTC_API/Perfect_negotiation

use std::sync::Arc;

use freecord_signal::{ClientMessage, PeerId, SignalPayload};
use tokio::sync::mpsc;
use webrtc::peer_connection::{PeerConnection, RTCIceCandidateInit, RTCSessionDescription};

/// Applies an incoming signaling message from `peer` to `pc`, sending back an
/// answer over `outbound` if the message was an offer. `polite` decides who
/// yields on an offer collision — see the module docs.
pub async fn handle(
    pc: &Arc<dyn PeerConnection>,
    outbound: &mpsc::UnboundedSender<ClientMessage>,
    peer: PeerId,
    payload: SignalPayload,
    polite: bool,
) -> anyhow::Result<()> {
    match payload {
        SignalPayload::Offer { sdp } => {
            // An outstanding `pending_local_description` means we've already
            // sent our own offer and are waiting on an answer — exactly the
            // "signalingState != stable" half of the spec's collision check.
            if pc.pending_local_description().await.is_some() {
                if !polite {
                    return Ok(());
                }
                pc.set_local_description(RTCSessionDescription::rollback(None)?)
                    .await?;
            }
            pc.set_remote_description(RTCSessionDescription::offer(sdp)?)
                .await?;
            let answer = pc.create_answer(None).await?;
            pc.set_local_description(answer.clone()).await?;
            outbound.send(ClientMessage::Relay {
                to: peer,
                payload: SignalPayload::Answer { sdp: answer.sdp },
            })?;
        }
        SignalPayload::Answer { sdp } => {
            pc.set_remote_description(RTCSessionDescription::answer(sdp)?)
                .await?;
        }
        SignalPayload::IceCandidate {
            candidate,
            sdp_mid,
            sdp_mline_index,
        } => {
            pc.add_ice_candidate(RTCIceCandidateInit {
                candidate,
                sdp_mid,
                sdp_mline_index,
                ..Default::default()
            })
            .await?;
        }
    }
    Ok(())
}
