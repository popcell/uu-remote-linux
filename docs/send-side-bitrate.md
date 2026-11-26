# Send-side bitrate response

## Problem and cause

Symptom: the viewer's picture freezes while the desktop keeps reacting to its clicks. Capture is
healthy — `Portal frames: new=897 repeated=4 window_s=30` and `Desktop latest-frame stats: sent=900`
keep counting — so the frames leave the host and never arrive. The viewer's own report agrees:
`avg_received_fps` decays 29 → 26 → 24 → 22 → 0 while the data channel (input) still works.

Cause: webrtc-rs 0.10 has no congestion control. An overloaded path only produces NACKs, and the
retransmissions multiply the traffic until nothing gets through. Measured on a hotel Wi-Fi relay path
with 6 Mbit/s requested:

```
21:33:32 Video receiver report: lost=0     fraction_lost=0   rtt=17ms    nack=16
21:33:33 Video receiver report: lost=425   fraction_lost=25  rtt=123ms   nack=10559
21:33:52 Video receiver report: lost=4746  fraction_lost=25  rtt=1701ms  nack=142733
21:34:12 Video receiver report: lost=26677 fraction_lost=25   rtt=2729ms  nack=261952
21:34:02 ICE path: tx_bps=Some(45780471)                                 ← 0.8-2 Mbit/s when healthy
```

Two facts matter for the fix: the desktop is still captured and encoded the whole time (so this is
not a capture or privacy-screen problem), and the viewer's 自动清晰度 is not a substitute — it
translates "auto" into `quality=6, custom_bitrate=u64::MAX` (i.e. unlimited), so the host really does
push 20-30 Mbit/s until the path breaks, and only then does the client lower its preset. By that
point the RTT is seconds and the picture is already dead.

## Implementation

`src/video_governor.rs` keeps the encoder's bitrate cap at or below what RTCP says the path carries.
It reads the receiver reports the feedback task already parses and, when the cap changes, writes
`VideoSettings.net_kbps`. The in-process VAAPI engine retunes its rate control in place: context,
surfaces and imported buffer stay, the change lands on the next IDR (`vaapi_set_rate`), and the
stream loop reports the retune through `VideoTrack::note_rate_applied` so the governor's climb gate
does not wait for a pipeline restart that never comes. Every other pipeline goes through the
settings watch and its normal `reader.restart` path (RTP timeline preserved, first frame is an IDR).

- Signals: a `fraction_lost >= 8%` window that really lost packets, or a round trip that stayed over
  300 ms (or 250 ms above the session's lowest sample) for two seconds.
  - A fraction alone is not evidence: a 250 ms report window on a sparse desktop stream (15-20 fps,
    one RTP packet per frame) holds 3-12 packets, so one reordered or NACK-recovered packet reads as
    33.2 % (85/256), 24.6 % (63/256) or 8.2 % (21/256). Measured on a 17-20 ms direct link whose every
    report said `lost=0`: those fractions halved the cap about 60 times in 29 minutes, drove an
    8 Mbit/s stream to 810k and restarted the encoder each time. The growth of RFC 3550's cumulative
    lost count is what separates that from congestion, so a window must lose at least
    `MIN_CONGESTION_LOST_PACKETS` (5) before its fraction counts; a path that breaks loses dozens.
  - The round trip is noisy in single samples (measured 563 ms and 1300 ms on an 18 ms link). It is
    confirmed by how long it stays over the knee, not by a report count, because the report cadence
    belongs to the viewer. The sample itself is computed on a monotonic base so an NTP step does not
    read as queueing.
  - NACK counts are deliberately **not** a trigger: they arrive in bursts (hundreds within 100 ms on a
    clean path), so an instantaneous rate reads as congestion on a perfect link.
- Decrease: halve on loss, 15 % on a round trip alone (queueing is not loss), floor 800 kbit/s, at most
  every 3 s. The rate that broke the path is remembered and recovery aims at 90 % of it, so a path
  whose capacity is 3 Mbit/s does not retry 3.1 Mbit/s every twenty seconds.
- Increase: +25% (at least 500 kbit/s) every 4 s, never above the aim. After 60 s without loss the aim
  is ignored once, which is the only way to discover that the path improved.
- Ceiling: the viewer's own choice (`max_kbps`) stays the ceiling; when it is absent or unlimited, the
  ceiling is `AUTO_CEILING_KBPS` (8 Mbit/s) instead of the encoder default. The viewer therefore stays
  in charge of 画质, and this only lowers it when the path actually loses packets.
- Between the knee and the cliff (1-8% loss, 150-300 ms RTT) nothing changes, so a marginal path does
  not flap the encoder.

Fields on the wire are untouched: `net_kbps` is local to the connection, never sent, and the viewer's
own settings keep their meaning (`requested_kbps()` vs `max_kbps()`).

## Verification

`cargo test --bin uuremote-server video_governor` covers the halving sequence down to the floor, a
swollen RTT alone, 3.5% and 6% jitter leaving the cap alone, recovery stopping at 90% of the broken
rate, the 60 s probe reaching the viewer ceiling again, an unlimited viewer choice being capped at
8 Mbit/s, and windows of one lost packet in three, four or twelve (33.2 %, 24.6 %, 8.2 %) leaving the
cap alone. `webrtc::network_tests` covers the window count, the wall-clock step and the NACK
attribution.

The live rate retune has a hardware acceptance test:
`cargo test --features inproc-av --bin uuremote-server -- --ignored vaapi_live_rate`. It encodes
noise at 2 Mbit/s, retunes the running engine to 8 Mbit/s and requires more than 1.5x the bitrate
with only the retune's keyframe, no pipeline restart. Measured on Intel Gen12 LP HEVC (UHD 730, iHD):
2.9 -> 5.9 Mbit/s. On that driver a rate change only takes effect at an IDR - a resubmitted
rate-control or sequence buffer on a low-delay P frame moved the output by 4 % - so
`vaapi_set_rate` arms one keyframe instead of the engine teardown this replaced.

Field check: with a 1-3 Mbit/s mobile path the log shows `Video bitrate cap` only after real loss
(12-27%) and the picture recovers instead of freezing; at 8 Mbit/s over a good path the governor never
fires. Diagnose a fresh freeze by the two lines above: `Portal frames: new=0` means the compositor
stopped painting (capture side), `new≈30` with `sent` climbing and no frames at the viewer means the
transport.

## Tuning

The thresholds are constants at the top of `src/video_governor.rs`
(`CONGESTION_LOSS_PERCENT`, `MIN_CONGESTION_LOST_PACKETS`, `RTT_CONFIRM_AFTER`,
`CLEAN_LOSS_PERCENT`, `DECREASE_AFTER`, `INCREASE_AFTER`, `PROBE_AFTER`, `FLOOR_KBPS`,
`AUTO_CEILING_KBPS`) and `MIN_CONGESTION_LOST_PACKETS`/`loss_window` in `src/webrtc.rs`. Raising the
loss threshold means fewer keyframes and longer tolerance of jitter; lowering it reacts earlier at
the cost of a visible hiccup (a step costs one keyframe on the live-retune path, and a full encoder
restart on the pipelines that cannot retune).
