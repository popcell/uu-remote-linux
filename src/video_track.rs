//! One negotiated video m-line/SSRC, with codec-specific payload types.
//! A single RTP sequence/timestamp survives format changes (never two independent SSRC timelines).
use std::{any::Any, sync::atomic::{AtomicBool, Ordering}};
use async_trait::async_trait;
use tokio::sync::Mutex;
use webrtc::{error::{Error, Result}, media::Sample, rtp::{packet::Packet, header::Header, packetizer::Payloader}};
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType};
use webrtc::track::track_local::{TrackLocal, TrackLocalContext, track_local_static_rtp::TrackLocalStaticRTP};
use crate::capture_setting::VideoCodec;

/// Official SDK GetSupportedFormats constructs H265 with an empty parameter map.
pub fn register_hevc(media: &mut webrtc::api::media_engine::MediaEngine) -> Result<()> {
    media.register_codec(RTCRtpCodecParameters { capability: RTCRtpCodecCapability {
        mime_type: VideoCodec::H265.mime().into(), clock_rate: 90_000,
        rtcp_feedback: vec![webrtc::rtp_transceiver::RTCPFeedback { typ: "nack".into(), parameter: String::new() },
            webrtc::rtp_transceiver::RTCPFeedback { typ: "nack".into(), parameter: "pli".into() }],
        ..Default::default() }, payload_type: 126, ..Default::default() }, RTPCodecType::Video)
}

#[derive(Debug)]
struct Timeline { sequence: u16, timestamp: u32, h264: webrtc::rtp::codecs::h264::H264Payloader }

/// One transmitted access unit's packet span, for attributing retransmission requests: the viewer
/// asks for missing packets, and what matters is whether they sit in a keyframe or in a frame
/// whose packet burst outran the path.
#[derive(Clone, Copy, Default, Debug)]
struct AuSpan { first: u16, last: u16, keyframe: bool }
#[derive(Default, Debug)]
struct SentFraming { ring: std::collections::VecDeque<AuSpan>, aus: u64, keyframe_aus: u64, packets: u64, keyframe_packets: u64 }
/// Sent-side framing counters (a snapshot for the log line; the producer parts of the protocol are
/// always the pacer's own thread, so a plain mutex carries no await).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Framing { pub aus: u64, pub keyframe_aus: u64, pub packets: u64, pub keyframe_packets: u64 }
/// Where a retransmission request's sequence number sat when it arrived.
pub enum NackClass { Keyframe, Other, Stale }
const AU_RING: usize = 64;

#[derive(Debug)]
pub struct VideoTrack {
    h264: TrackLocalStaticRTP, h265: TrackLocalStaticRTP,
    has_h264: AtomicBool, has_h265: AtomicBool, timeline: Mutex<Timeline>,
    progress: tokio::sync::watch::Sender<(u64, VideoCodec)>,
    framing: std::sync::Mutex<SentFraming>,
    /// Encoder configuration of the last complete AU that went out. A restart bumps it, so the
    /// first AU of the new pipeline is distinguishable from one the replaced pipeline had
    /// already queued.
    generation: std::sync::atomic::AtomicU64,
    /// Rate-control retunes the producing engine had applied for the last complete AU. The pipeline
    /// keeps its generation across a retune, so this is how a retune becomes visible to the governor.
    rate_epoch: std::sync::atomic::AtomicU64,
}
impl VideoTrack {
    pub fn new() -> Self {
        let track = |codec: VideoCodec| TrackLocalStaticRTP::new(
            RTCRtpCodecCapability { mime_type: codec.mime().into(), clock_rate: 90_000, ..Default::default() },
            "video_0".into(), "video_0".into());
        let (progress, _) = tokio::sync::watch::channel((0, VideoCodec::H264));
        Self { progress, h264: track(VideoCodec::H264), h265: track(VideoCodec::H265), has_h264: AtomicBool::new(false),
            has_h265: AtomicBool::new(false), timeline: Mutex::new(Timeline { sequence: rand::random(), timestamp: rand::random(), h264: Default::default() }),
            framing: std::sync::Mutex::new(SentFraming::default()),
            generation: std::sync::atomic::AtomicU64::new(0),
            rate_epoch: std::sync::atomic::AtomicU64::new(0) }
    }
    pub fn supports(&self, codec: VideoCodec) -> bool {
        match codec { VideoCodec::H264 => &self.has_h264, VideoCodec::H265 => &self.has_h265 }.load(Ordering::Acquire)
    }
    pub fn revision(&self) -> u64 { self.progress.borrow().0 }
    /// Encoder configuration generation of the last complete AU written to the wire.
    pub fn generation(&self) -> u64 { self.generation.load(Ordering::Acquire) }
    /// Rate-control retunes the engine that produced the last complete AU had applied.
    pub fn rate_epoch(&self) -> u64 { self.rate_epoch.load(Ordering::Acquire) }
    pub(crate) async fn next_timestamp(&self) -> u32 { self.timeline.lock().await.timestamp }
    /// Records one transmitted access unit's packet span for retransmission attribution.
    fn note_span(&self, first: u16, count: usize, keyframe: bool) {
        let mut sent = self.framing.lock().unwrap_or_else(|e| e.into_inner());
        sent.aus += 1;
        sent.packets += count as u64;
        if keyframe { sent.keyframe_aus += 1; sent.keyframe_packets += count as u64; }
        if sent.ring.len() == AU_RING { sent.ring.pop_front(); }
        sent.ring.push_back(AuSpan { first, last: first.wrapping_add(count as u16 - 1), keyframe });
    }
    pub fn framing(&self) -> Framing {
        let sent = self.framing.lock().unwrap_or_else(|e| e.into_inner());
        Framing { aus: sent.aus, keyframe_aus: sent.keyframe_aus, packets: sent.packets, keyframe_packets: sent.keyframe_packets }
    }
    /// Which kind of frame a retransmission request asked for. A sequence number older than the
    /// ring (a request that arrived long after the frame) is `Stale`.
    pub fn classify_nack(&self, sequence: u16) -> NackClass {
        let sent = self.framing.lock().unwrap_or_else(|e| e.into_inner());
        for span in sent.ring.iter().rev() {
            if sequence.wrapping_sub(span.first) <= span.last.wrapping_sub(span.first) {
                return if span.keyframe { NackClass::Keyframe } else { NackClass::Other };
            }
        }
        NackClass::Stale
    }
    /// Format-change RPC success follows a transmitted frame, including a negotiated fallback.
    pub async fn wait_for_applied(&self, since: u64, settings: &tokio::sync::watch::Sender<crate::capture_setting::VideoSettings>) -> bool {
        let mut progress = self.progress.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let (revision, codec) = *progress.borrow_and_update();
                if revision > since && codec == settings.borrow().codec { return true; }
                if progress.changed().await.is_err() { return false; }
            }
        }).await.unwrap_or(false)
    }
    #[cfg(test)]
    pub(crate) fn test_set_h265(&self, supported: bool) {
        self.has_h265.store(supported, Ordering::Release);
    }
    #[cfg(test)]
    pub(crate) fn test_note_applied(&self, codec: VideoCodec) {
        self.progress.send_modify(|value| { value.0 += 1; value.1 = codec; });
    }
    pub async fn write_sample_with_extensions(&self, codec: VideoCodec, sample: &Sample, extensions: &[webrtc::rtp::extension::HeaderExtension], generation: u64, rate_epoch: u64) -> Result<()> {
        self.write_sample_with_timestamp(codec, sample, extensions, None, generation, rate_epoch).await
    }
    pub async fn write_sample_with_extensions_at(&self, codec: VideoCodec, sample: &Sample, extensions: &[webrtc::rtp::extension::HeaderExtension], timestamp: u32, generation: u64, rate_epoch: u64) -> Result<()> {
        self.write_sample_with_timestamp(codec, sample, extensions, Some(timestamp), generation, rate_epoch).await
    }
    async fn write_sample_with_timestamp(&self, codec: VideoCodec, sample: &Sample, extensions: &[webrtc::rtp::extension::HeaderExtension], timestamp_override: Option<u32>, generation: u64, rate_epoch: u64) -> Result<()> {
        if !self.supports(codec) { return Err(Error::ErrUnsupportedCodec); }
        let track = match codec { VideoCodec::H264 => &self.h264, VideoCodec::H265 => &self.h265 };
        if track.all_binding_paused().await { return Ok(()); }
        let mut state = self.timeline.lock().await;
        // Leave room for the RTP header and negotiated UU extensions below the 1200 byte MTU.
        let payloads = match codec {
            VideoCodec::H264 => state.h264.payload(1160, &sample.data).map_err(Error::Rtp)?,
            VideoCodec::H265 => crate::hevc::payload_bytes(1160, &sample.data).map_err(Error::new)?,
        };
        let count = payloads.len();
        let mut complete = count > 0;
        let timestamp = timestamp_override.unwrap_or(state.timestamp);
        let first = state.sequence;
        state.timestamp = timestamp.wrapping_add((sample.duration.as_secs_f64() * 90_000.0) as u32);
        // Publish the span before the writes: a retransmission request for an early packet of a large
        // access unit arrives while the rest is still being handed to the transport.
        if count > 0 { self.note_span(first, count, crate::video::is_independent_frame_for(&sample.data, codec)); }
        for (index, payload) in payloads.into_iter().enumerate() {
            let packet = Packet { header: Header { version: 2, sequence_number: state.sequence, timestamp,
                marker: index + 1 == count, ..Default::default() }, payload };
            state.sequence = state.sequence.wrapping_add(1);
            complete &= track.write_rtp_with_extensions(&packet, extensions).await? > 0;
        }
        if complete {
            self.generation.store(generation, Ordering::Release);
            // Monotonic so synthetic frames (e.g. locked-desktop black video carrying 0) never
            // regress the governor's confirmed rate revision.
            self.rate_epoch.fetch_max(rate_epoch, Ordering::AcqRel);
            self.progress.send_modify(|value| { value.0 += 1; value.1 = codec; });
        }
        Ok(())
    }
}
#[async_trait]
impl TrackLocal for VideoTrack {
    async fn bind(&self, context: &TrackLocalContext) -> Result<RTCRtpCodecParameters> {
        let h264 = self.h264.bind(context).await.ok();
        let h265 = self.h265.bind(context).await.ok();
        self.has_h264.store(h264.is_some(), Ordering::Release);
        self.has_h265.store(h265.is_some(), Ordering::Release);
        log::info!("Video track negotiated: h264={} h265={}", h264.is_some(), h265.is_some());
        h264.or(h265).ok_or(Error::ErrUnsupportedCodec)
    }
    async fn unbind(&self, context: &TrackLocalContext) -> Result<()> {
        if self.has_h264.swap(false, Ordering::AcqRel) { self.h264.unbind(context).await?; }
        if self.has_h265.swap(false, Ordering::AcqRel) { self.h265.unbind(context).await?; }
        Ok(())
    }
    fn id(&self) -> &str { "video_0" }
    fn stream_id(&self) -> &str { "video_0" }
    fn kind(&self) -> RTPCodecType { RTPCodecType::Video }
    fn as_any(&self) -> &dyn Any { self }
}

#[cfg(test)]
mod tests {
    use super::*;
    use webrtc::{api::{APIBuilder, media_engine::MediaEngine}, peer_connection::configuration::RTCConfiguration};
    async fn negotiate(h264: bool, hevc: bool) {
        let mut remote_media = MediaEngine::default();
        if h264 { remote_media.register_codec(RTCRtpCodecParameters { capability: RTCRtpCodecCapability {
            mime_type: VideoCodec::H264.mime().into(), clock_rate: 90_000,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".into(), ..Default::default()
        }, payload_type: 102, ..Default::default() }, RTPCodecType::Video).unwrap(); }
        if hevc { register_hevc(&mut remote_media).unwrap(); }
        let (remote_setting, host_setting, router) = crate::test_network::virtual_network().await;
        let remote = APIBuilder::new().with_setting_engine(remote_setting).with_media_engine(remote_media).build().new_peer_connection(RTCConfiguration::default()).await.unwrap();
        remote.add_transceiver_from_kind(RTPCodecType::Video, None).await.unwrap();
        let mut media = MediaEngine::default(); media.register_default_codecs().unwrap(); register_hevc(&mut media).unwrap();
        let host = APIBuilder::new().with_setting_engine(host_setting).with_media_engine(media).build().new_peer_connection(RTCConfiguration::default()).await.unwrap();
        let (packets_tx, mut packets_rx) = tokio::sync::mpsc::unbounded_channel();
        remote.on_track(Box::new(move |remote_track, _, _| {
            let packets_tx = packets_tx.clone();
            Box::pin(async move {
                tokio::spawn(async move {
                    while let Ok((packet, _)) = remote_track.read_rtp().await {
                        if packets_tx.send(packet).is_err() { break; }
                    }
                });
            })
        }));
        let track = std::sync::Arc::new(VideoTrack::new());
        host.add_track(track.clone()).await.unwrap();
        let offer = remote.create_offer(None).await.unwrap();
        let mut gathered = remote.gathering_complete_promise().await;
        remote.set_local_description(offer).await.unwrap();
        gathered.recv().await;
        host.set_remote_description(remote.local_description().await.unwrap()).await.unwrap();
        let answer = host.create_answer(None).await.unwrap();
        assert_eq!(answer.sdp.contains("H265/90000"), hevc);
        assert_eq!(answer.sdp.contains("H264/90000"), h264);
        let mut gathered = host.gathering_complete_promise().await;
        host.set_local_description(answer).await.unwrap();
        gathered.recv().await;
        remote.set_remote_description(host.local_description().await.unwrap()).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !track.supports(VideoCodec::H264) && !track.supports(VideoCodec::H265) { tokio::time::sleep(std::time::Duration::from_millis(10)).await; }
        }).await.unwrap();
        assert_eq!(track.supports(VideoCodec::H264), h264);
        assert_eq!(track.supports(VideoCodec::H265), hevc);
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while host.connection_state() != webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Connected {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        let codecs = if !h264 { vec![VideoCodec::H265] } else if hevc { vec![VideoCodec::H264, VideoCodec::H265, VideoCodec::H264] } else { vec![VideoCodec::H264] };
        let mut previous: Option<Packet> = None;
        for codec in codecs {
            let data = match codec { VideoCodec::H264 => bytes::Bytes::from_static(&[0,0,1,65,1,2]),
                VideoCodec::H265 => bytes::Bytes::from_static(&[0,0,1,38,1,2]) };
            let (settings, _) = tokio::sync::watch::channel(crate::capture_setting::VideoSettings { codec, ..Default::default() });
            let applied = track.wait_for_applied(track.revision(), &settings);
            tokio::pin!(applied);
            assert!(tokio::time::timeout(std::time::Duration::from_millis(1), &mut applied).await.is_err());
            track.write_sample_with_extensions(codec, &Sample { data, duration: std::time::Duration::from_millis(20), ..Default::default() }, &[], 1, 0).await.unwrap();
            assert!(applied.await);
            let packet = tokio::time::timeout(std::time::Duration::from_secs(3), packets_rx.recv()).await.unwrap().unwrap();
            assert_eq!(packet.header.payload_type, if codec == VideoCodec::H264 { 102 } else { 126 });
            assert!(packet.header.marker);
            if let Some(previous) = previous {
                assert_eq!(packet.header.ssrc, previous.header.ssrc);
                assert_eq!(packet.header.sequence_number, previous.header.sequence_number.wrapping_add(1));
                assert_eq!(packet.header.timestamp, previous.header.timestamp.wrapping_add(1800));
            }
            previous = Some(packet);
        }
        host.close().await.unwrap(); remote.close().await.unwrap();
        router.lock().await.stop().await.unwrap();
    }
    #[tokio::test]
    async fn negotiated_codecs_only_and_legacy_h264_offer() { negotiate(true, false).await; negotiate(true, true).await; negotiate(false, true).await; }

    #[test]
    fn retransmission_requests_are_attributed_to_the_frame_that_lost_them() {
        let track = VideoTrack::new();
        track.note_span(100, 30, true);
        track.note_span(130, 4, false);
        assert_eq!((track.generation(), track.framing().aus, track.framing().keyframe_aus), (0, 2, 1));
        assert_eq!((track.framing().packets, track.framing().keyframe_packets), (34, 30));
        assert!(matches!(track.classify_nack(100), NackClass::Keyframe));
        assert!(matches!(track.classify_nack(129), NackClass::Keyframe));
        assert!(matches!(track.classify_nack(130), NackClass::Other));
        assert!(matches!(track.classify_nack(133), NackClass::Other));
        // Older than the ring: the request arrived long after the frame.
        assert!(matches!(track.classify_nack(99), NackClass::Stale));
        // A span that wrapped the sequence space still contains its packets.
        track.note_span(u16::MAX - 1, 4, false);
        assert!(matches!(track.classify_nack(u16::MAX), NackClass::Other));
        assert!(matches!(track.classify_nack(1), NackClass::Other));
        assert_eq!(track.framing().aus, 3);
    }
}
