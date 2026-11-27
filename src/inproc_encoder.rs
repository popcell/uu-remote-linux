//! In-process X11 capture (XDamage + XShm) and NVENC encoding through the NVIDIA driver.
//! With UU_VIDEO_ENCODER=vaapi, H.264 / HEVC go through VAAPI instead: a DRI3 DMA-BUF of the root window is
//! imported as a VA surface and converted/scaled by VPP, so the CPU never touches the pixels (XShm upload
//! when DRI3 is unavailable).
//! Portal sessions can instead upload their existing BGRA frames, sharing the same GPU VPP and encoder.
//! When capture and encoder sizes match, the XShm buffer is registered as CUDA pinned memory and
//! NVENC reads it by DMA, so there is no host-to-device copy (the X server still writes XShm).

use std::collections::VecDeque;
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};

use crate::damage_capture::DamageCapture;

#[repr(C)]
struct NvencEngineOpaque {
    _private: [u8; 0],
}

#[repr(C)]
struct NvencConfigFFI {
    width: u32,
    height: u32,
    fps: u32,
    gop: u32,
    cq: u32,
    target_bps: i64,
    max_bps: i64,
    buffer_bits: i64,
    is_hevc: libc::c_int,
    chroma_444: libc::c_int,
}

#[repr(C)]
struct VaapiEngineOpaque {
    _private: [u8; 0],
}

#[repr(C)]
struct VaapiConfigFFI {
    width: u32,
    height: u32,
    cap_width: u32,
    cap_height: u32,
    fps: u32,
    gop: u32,
    cq: u32,
    target_bps: i64,
    max_bps: i64,
    buffer_bits: i64,
    is_hevc: libc::c_int,
    device: *const libc::c_char,
}

#[repr(C)]
struct Dri3CaptureOpaque {
    _private: [u8; 0],
}

extern "C" {
    fn nvenc_create(config: *const NvencConfigFFI) -> *mut NvencEngineOpaque;
    fn nvenc_register_host_buffer(
        engine: *mut NvencEngineOpaque,
        host_ptr: *mut libc::c_void,
        size: usize,
    ) -> libc::c_int;
    fn nvenc_encode_frame(
        engine: *mut NvencEngineOpaque,
        input_argb: *const u8,
        pitch: u32,
        force_idr: libc::c_int,
        out_data: *mut *const u8,
        out_len: *mut u32,
    ) -> libc::c_int;
    fn nvenc_destroy(engine: *mut NvencEngineOpaque);

    fn vaapi_create(config: *const VaapiConfigFFI) -> *mut VaapiEngineOpaque;
    fn vaapi_import_dma_buffer(engine: *mut VaapiEngineOpaque, dma_fd: libc::c_int, stride: u32) -> libc::c_int;
    fn vaapi_import_frame(engine: *mut VaapiEngineOpaque, frame: *const crate::portal::DmaFrame) -> libc::c_int;
    fn vaapi_upload_host_buffer(engine: *mut VaapiEngineOpaque, host_ptr: *const u8, pitch: u32) -> libc::c_int;
    fn vaapi_supports_rate_control(engine: *const VaapiEngineOpaque) -> libc::c_int;
    fn vaapi_supports_bounded_sync(engine: *const VaapiEngineOpaque) -> libc::c_int;
    fn vaapi_convert_input(engine: *mut VaapiEngineOpaque, release_input: libc::c_int) -> libc::c_int;
    fn vaapi_encode_nv12(
        engine: *mut VaapiEngineOpaque,
        force_idr: libc::c_int,
        count_gop: libc::c_int,
        out_data: *mut *const u8,
        out_len: *mut u32,
    ) -> libc::c_int;
    fn vaapi_encode_frame(
        engine: *mut VaapiEngineOpaque,
        force_idr: libc::c_int,
        out_data: *mut *const u8,
        out_len: *mut u32,
    ) -> libc::c_int;
    /// Retunes rate control; the engine keeps its context and surfaces, so the change lands on the
    /// next IDR - one keyframe instead of the pipeline restart this replaces.
    fn vaapi_set_rate(
        engine: *mut VaapiEngineOpaque,
        target_bps: i64,
        max_bps: i64,
        buffer_bits: i64,
        cq: u32,
    );
    fn vaapi_destroy(engine: *mut VaapiEngineOpaque);

    fn dri3_capture_create(display_name: *const libc::c_char, width: u32, height: u32) -> *mut Dri3CaptureOpaque;
    fn dri3_capture_blit(cap: *mut Dri3CaptureOpaque) -> libc::c_int;
    fn dri3_capture_wait_damage(cap: *mut Dri3CaptureOpaque, timeout_ms: libc::c_int) -> libc::c_int;
    fn dri3_capture_get_dma_fd(cap: *mut Dri3CaptureOpaque) -> libc::c_int;
    fn dri3_capture_get_stride(cap: *mut Dri3CaptureOpaque) -> u32;
    fn dri3_capture_destroy(cap: *mut Dri3CaptureOpaque);
}

struct SafeNvencEngine(*mut NvencEngineOpaque);

unsafe impl Send for SafeNvencEngine {}

impl Drop for SafeNvencEngine {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { nvenc_destroy(self.0) };
            self.0 = std::ptr::null_mut();
        }
    }
}

struct SafeVaapiEngine(*mut VaapiEngineOpaque);
unsafe impl Send for SafeVaapiEngine {}
impl Drop for SafeVaapiEngine {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { vaapi_destroy(self.0) };
            self.0 = std::ptr::null_mut();
        }
    }
}

struct SafeDri3Capture(*mut Dri3CaptureOpaque);
unsafe impl Send for SafeDri3Capture {}
impl Drop for SafeDri3Capture {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { dri3_capture_destroy(self.0) };
            self.0 = std::ptr::null_mut();
        }
    }
}

pub struct Config {
    pub portal: Option<Arc<crate::portal::PortalSession>>,
    pub display: String,
    pub input: (u32, u32),
    pub output: Option<(u32, u32)>,
    pub fps: u32,
    pub gop: u32,
    pub cq: u32,
    pub target_bps: i64,
    pub max_bps: i64,
    pub buffer_bits: i64,
    pub codec: crate::capture_setting::VideoCodec,
    /// ChromaFormat (capture_setting::CHROMA_444 = 4:4:4).
    pub chroma: u64,
    /// UU_VIDEO_ENCODER=vaapi: the VAAPI engine (H.264 / HEVC Main) instead of NVENC.
    pub vaapi: bool,
    /// Governor revision this pipeline started with (carried onto EncodedUnit.rate_epoch).
    pub rate_revision: u64,
    /// Live rate control: the connection's `RateHandle`, so a bitrate/quality step retunes this
    /// engine instead of restarting the pipeline.
    pub rate: Option<Arc<crate::video::RateHandle>>,
}

struct RateActiveGuard(Option<Arc<crate::video::RateHandle>>);
impl RateActiveGuard {
    fn activate(rate: Option<&Arc<crate::video::RateHandle>>, supported: bool) -> Self {
        if supported {
            if let Some(rate) = rate {
                rate.activate();
                return Self(Some(rate.clone()));
            }
        }
        Self(None)
    }
}
impl Drop for RateActiveGuard {
    fn drop(&mut self) {
        if let Some(rate) = &self.0 { rate.deactivate(); }
    }
}

/// Pushes a changed rate into the running engine once per frame and tracks the governor revision
/// this engine has applied. Returns silently when the handle is absent or nothing changed.
fn sync_rate(
    engine: &SafeVaapiEngine,
    rate: Option<&Arc<crate::video::RateHandle>>,
    applied: &mut Option<crate::video::Rate>,
    applied_revision: &mut u64,
) {
    let Some(rate) = rate else { return };
    let (wanted, revision) = rate.get();
    if *applied != Some(wanted) {
        unsafe { vaapi_set_rate(engine.0, wanted.target_bps, wanted.max_bps, wanted.buffer_bits, wanted.cq) };
        log::info!("VAAPI rate control: target={}k max={}k buffer={}k min_qp={} (in place)",
            wanted.target_bps / 1000, wanted.max_bps / 1000, wanted.buffer_bits / 1000, wanted.cq);
        *applied = Some(wanted);
    }
    *applied_revision = (*applied_revision).max(revision);
}

pub fn disabled() -> bool {
    std::env::var("UU_VIDEO_PIPELINE").is_ok_and(|v| v == "process")
}

pub fn damage_enabled() -> bool {
    std::env::var("UU_CAPTURE").map_or(true, |v| v != "grab")
}

pub fn selected(encoder: crate::video::Encoder) -> bool {
    matches!(encoder, crate::video::Encoder::Nvenc | crate::video::Encoder::Vaapi) && damage_enabled() && !disabled() && crate::config::ffmpeg_root().is_none()
}

/// One encoder thread per stream. Aborting the reader only asks the blocking thread to stop at its
/// next wake-up, so a restart waits here until the previous NVENC session and XShm are released.
/// The key is the stream's IDR flag, which survives encoder restarts.
pub async fn stream_slot(stream: &Arc<AtomicBool>) -> tokio::sync::OwnedMutexGuard<()> {
    type Slots = Vec<(std::sync::Weak<AtomicBool>, Arc<tokio::sync::Mutex<()>>)>;
    static SLOTS: std::sync::Mutex<Slots> = std::sync::Mutex::new(Vec::new());
    let slot = {
        let mut slots = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        slots.retain(|(key, _)| key.strong_count() > 0);
        match slots.iter().find(|(key, _)| std::ptr::eq(key.as_ptr(), Arc::as_ptr(stream))) {
            Some((_, slot)) => slot.clone(),
            None => {
                let slot = Arc::new(tokio::sync::Mutex::new(()));
                slots.push((Arc::downgrade(stream), slot.clone()));
                slot
            }
        }
    };
    slot.lock_owned().await
}

pub struct EncodedUnit {
    pub data: Vec<u8>,
    pub permit: OwnedSemaphorePermit,
    pub completed_at: Instant,
    /// Rate-control retunes this engine has applied, so the send side can tell an AU from one
    /// encoded before the retune (see `video::RateHandle`).
    pub rate_epoch: u64,
}

fn acquire(
    budget: &Arc<Semaphore>,
    units: &mpsc::Sender<EncodedUnit>,
) -> Option<OwnedSemaphorePermit> {
    tokio::runtime::Handle::current().block_on(async {
        tokio::select! {
            permit = budget.clone().acquire_owned() => permit.ok(),
            _ = units.closed() => None,
        }
    })
}

/// Capture-to-send segmentation for the Portal encoder, logged once a window so a stall names its
/// own stage instead of only showing up as frame loss (duration and counts only).
#[derive(Default)]
struct CaptureStats {
    window: Option<Instant>,
    frames: u64,
    /// Frame arrival -> rate gate open, permit backpressure, VPP copy (PipeWire lease hold), encode.
    /// All four are added only for an AU that was produced, so the averages describe one population.
    gate: Duration, gate_max: Duration,
    vpp: Duration, vpp_max: Duration,
    encode: Duration, encode_max: Duration,
    permit: Duration, permit_max: Duration,
    gate_last: Duration, vpp_last: Duration, permit_last: Duration,
}
impl CaptureStats {
    /// A pass without a fresh compositor frame (a keepalive re-encode) has no arrival to measure.
    fn gate(&mut self, arrived: Option<Instant>) { self.gate_last = arrived.map_or(Duration::ZERO, |at| at.elapsed()); }
    fn permit(&mut self, asked: Instant) { self.permit_last = asked.elapsed(); }
    fn vpp(&mut self, started: Instant) { self.vpp_last = started.elapsed(); }
    fn vpp_skipped(&mut self) { self.vpp_last = Duration::ZERO; }
    fn encoded(&mut self, started: Instant) {
        let took = started.elapsed();
        self.gate += self.gate_last; self.gate_max = self.gate_max.max(self.gate_last);
        self.vpp += self.vpp_last; self.vpp_max = self.vpp_max.max(self.vpp_last);
        self.permit += self.permit_last; self.permit_max = self.permit_max.max(self.permit_last);
        self.encode += took; self.encode_max = self.encode_max.max(took); self.frames += 1;
        let window = *self.window.get_or_insert_with(Instant::now);
        if window.elapsed() < Duration::from_secs(10) { return; }
        let frames = self.frames.max(1) as f64;
        log::info!("Portal encode stats: window_s={} frames={} gate_ms_avg={:.2} gate_ms_max={} vpp_ms_avg={:.2} vpp_ms_max={} encode_ms_avg={:.2} encode_ms_max={} permit_ms_avg={:.2} permit_ms_max={}",
            window.elapsed().as_secs(), self.frames,
            self.gate.as_secs_f64() * 1000.0 / frames, self.gate_max.as_millis(),
            self.vpp.as_secs_f64() * 1000.0 / frames, self.vpp_max.as_millis(),
            self.encode.as_secs_f64() * 1000.0 / frames, self.encode_max.as_millis(),
            self.permit.as_secs_f64() * 1000.0 / frames, self.permit_max.as_millis());
        *self = CaptureStats { window: Some(Instant::now()), ..Default::default() };
    }
}

fn random_keepalive(seed: &mut u32) -> Duration {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 17;
    *seed ^= *seed << 5;
    Duration::from_micros(40_000 + u64::from(*seed % 26_700))
}

pub fn run(
    config: Config,
    units: mpsc::Sender<EncodedUnit>,
    stop: Arc<AtomicBool>,
    idr: Arc<AtomicBool>,
    budget: Arc<Semaphore>,
) -> Result<(), String> {
    if config.vaapi {
        run_vaapi(config, units, stop, idr, budget)
    } else {
        run_nvenc(config, units, stop, idr, budget)
    }
}

pub fn black_frame(codec: crate::capture_setting::VideoCodec) -> Result<bytes::Bytes, String> {
    let device = CString::new(crate::video::vaapi_device()).map_err(|_| "Invalid VAAPI device")?;
    let config = VaapiConfigFFI {
        width: 1280, height: 720, cap_width: 1280, cap_height: 720, fps: 1, gop: 1, cq: 26,
        target_bps: 1_000_000, max_bps: 2_000_000, buffer_bits: 500_000,
        is_hevc: libc::c_int::from(codec == crate::capture_setting::VideoCodec::H265),
        device: device.as_ptr(),
    };
    let engine = SafeVaapiEngine(unsafe { vaapi_create(&config) });
    if engine.0.is_null() { return Err("Locked video VAAPI initialization failed".into()); }
    let pixels = vec![0; 1280 * 720 * 4];
    if unsafe { vaapi_upload_host_buffer(engine.0, pixels.as_ptr(), 1280 * 4) } != 0 {
        return Err("Locked video VAAPI upload failed".into());
    }
    let mut data = std::ptr::null();
    let mut length = 0;
    if unsafe { vaapi_encode_frame(engine.0, 1, &mut data, &mut length) } != 0 || data.is_null() || length == 0 {
        return Err("Locked video VAAPI encode failed".into());
    }
    Ok(bytes::Bytes::copy_from_slice(unsafe { std::slice::from_raw_parts(data, length as usize) }))
}

fn run_vaapi(
    config: Config,
    units: mpsc::Sender<EncodedUnit>,
    stop: Arc<AtomicBool>,
    idr: Arc<AtomicBool>,
    budget: Arc<Semaphore>,
) -> Result<(), String> {
    let (width, height) = config.output.unwrap_or(config.input);
    let (width, height) = (width + width % 2, height + height % 2);
    let (cap_width, cap_height) = config.input;
    log::info!(
        "Initializing Native In-Process VAAPI: {}x{} -> {}x{}, {} fps, GOP {}, HEVC={}",
        cap_width,
        cap_height,
        width,
        height,
        config.fps,
        config.gop,
        config.codec == crate::capture_setting::VideoCodec::H265
    );
    let device = CString::new(crate::video::vaapi_device()).map_err(|_| "UU_VAAPI_DEVICE contains NUL".to_string())?;
    let ffi_cfg = VaapiConfigFFI {
        width,
        height,
        cap_width,
        cap_height,
        fps: config.fps,
        gop: config.gop,
        cq: config.cq,
        target_bps: config.target_bps,
        max_bps: config.max_bps,
        buffer_bits: config.buffer_bits,
        is_hevc: libc::c_int::from(config.codec == crate::capture_setting::VideoCodec::H265),
        device: device.as_ptr(),
    };
    let raw_engine = unsafe { vaapi_create(&ffi_cfg) };
    if raw_engine.is_null() {
        return Err("Failed to create native VAAPI engine".into());
    }
    let engine = SafeVaapiEngine(raw_engine);
    let rc_supported = unsafe { vaapi_supports_rate_control(engine.0) != 0 };
    if !rc_supported && config.rate.is_some() {
        // Desktop streams carry a send-side bitrate governor; running in uncapped CQP would
        // ignore bitrate caps while stamping `config.rate_revision` as if applied. Fall back so
        // `read_encoder` selects a pipeline whose rate control honors caps (FFmpeg VBR / CPU).
        return Err("VAAPI driver only supports CQP rate control; falling back for bitrate capping".into());
    }
    let live_rate = rc_supported.then(|| config.rate.as_ref()).flatten().cloned();
    let _rate_guard = RateActiveGuard::activate(live_rate.as_ref(), rc_supported);
    let mut applied = Some(crate::video::Rate {
        target_bps: config.target_bps,
        max_bps: config.max_bps,
        buffer_bits: config.buffer_bits,
        cq: config.cq,
    });
    let mut applied_revision = config.rate_revision;
    sync_rate(&engine, live_rate.as_ref(), &mut applied, &mut applied_revision);
    if let Some(portal) = config.portal.as_ref() {
        return run_portal_vaapi(&config, portal, Some(engine), live_rate.as_ref(), units, stop, idr, budget, &mut applied, &mut applied_revision);
    }

    // Capture stays at the full input size; VPP scales (or pads) to the encoder size.
    let display = CString::new(config.display.as_str()).map_err(|_| "DISPLAY contains NUL".to_string())?;
    let dri3 = Some(SafeDri3Capture(unsafe { dri3_capture_create(display.as_ptr(), cap_width, cap_height) }))
        .filter(|c| !c.0.is_null())
        .filter(|c| unsafe {
            let (fd, stride) = (dri3_capture_get_dma_fd(c.0), dri3_capture_get_stride(c.0));
            fd >= 0 && stride > 0 && vaapi_import_dma_buffer(engine.0, fd, stride) == 0
        });
    let mut capture = None;
    if dri3.is_some() {
        log::info!("Native VAAPI imports a DRI3 DMA-BUF of the root window (GPU blit, VPP to NV12)");
    } else {
        log::warn!("DRI3 DMA-BUF unavailable on {}, uploading XShm frames to VAAPI", config.display);
        capture = Some(
            DamageCapture::open(&config.display)
                .map_err(|e| format!("DamageCapture::open failed on {}: {e}", config.display))?,
        );
    }

    let frame_interval = Duration::from_nanos(1_000_000_000 / u64::from(config.fps));
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| d.subsec_nanos())
        | 1;
    let mut keepalive = random_keepalive(&mut seed);
    let mut dirty = true;
    let mut next_tick = Instant::now();
    let mut last_send = Instant::now();
    let mut permits = VecDeque::with_capacity(4);

    let locked = crate::session_lock::watch_locked();
    let mut last_locked = *locked.borrow();
    let mut nv12_ready = false;

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        sync_rate(&engine, live_rate.as_ref(), &mut applied, &mut applied_revision);
        let deadline = if dirty {
            next_tick
        } else {
            last_send + keepalive
        };
        let timeout = deadline.saturating_duration_since(Instant::now());

        if vaapi_wait(&dri3, &mut capture, timeout.max(Duration::from_millis(1)))? {
            dirty = true;
        }

        let now = Instant::now();
        if dirty && now < next_tick {
            continue;
        }
        if !dirty && now < last_send + keepalive {
            continue;
        }

        let Some(permit) = acquire(&budget, &units) else {
            return Ok(());
        };
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        dirty |= vaapi_wait(&dri3, &mut capture, Duration::ZERO)?;
        let state = *locked.borrow();
        if state != last_locked {
            dirty = true;
            last_locked = state;
        }

        let force_idr = if idr.swap(false, Ordering::AcqRel) {
            1
        } else {
            0
        };

        // Keepalive frames re-encode the already-converted NV12 surface and do not advance GOP.
        if dirty || !nv12_ready {
            match (dri3.as_ref(), capture.as_mut()) {
                (Some(c), _) => {
                    if unsafe { dri3_capture_blit(c.0) } != 0 {
                        return Err("DRI3 blit of the root window failed".into());
                    }
                }
                (None, Some(c)) => {
                    let pitch = c.size().0 * 4;
                    let res = unsafe { vaapi_upload_host_buffer(engine.0, c.capture()?.as_ptr(), pitch) };
                    if res != 0 {
                        return Err(format!("vaapi_upload_host_buffer failed with error {res}"));
                    }
                }
                (None, None) => unreachable!(),
            }
            let conv = unsafe { vaapi_convert_input(engine.0, 0) };
            if conv != 0 {
                return Err(format!("vaapi_convert_input failed with error {conv}"));
            }
            nv12_ready = true;
        }

        let mut out_data: *const u8 = std::ptr::null();
        let mut out_len: u32 = 0;
        let res = unsafe { vaapi_encode_nv12(engine.0, force_idr, libc::c_int::from(dirty), &mut out_data, &mut out_len) };
        if res != 0 {
            return Err(format!("vaapi_encode_nv12 failed with error {res}"));
        }

        if out_len > 0 && !out_data.is_null() {
            let data = unsafe { std::slice::from_raw_parts(out_data, out_len as usize) }.to_vec();
            permits.push_back(permit);
            let send_permit = permits.pop_front().unwrap();

            if units
                .blocking_send(EncodedUnit {
                    data,
                    permit: send_permit,
                    completed_at: Instant::now(),
                    rate_epoch: applied_revision,
                })
                .is_err()
            {
                return Ok(());
            }
        }

        let now = Instant::now();
        if dirty {
            let next = next_tick + frame_interval;
            next_tick = if next > now {
                next
            } else {
                now + frame_interval
            };
            dirty = false;
        }
        last_send = now;
        keepalive = random_keepalive(&mut seed);
    }
}
fn vaapi_encode_nv12_packet(engine: &SafeVaapiEngine, idr: &AtomicBool, count_gop: bool) -> Result<Vec<u8>, String> {
    let mut data = std::ptr::null();
    let mut length = 0;
    let force_idr = libc::c_int::from(idr.swap(false, Ordering::AcqRel));
    let result = unsafe { vaapi_encode_nv12(engine.0, force_idr, libc::c_int::from(count_gop), &mut data, &mut length) };
    if result != 0 { return Err(format!("VAAPI encode failed: {result}")); }
    if data.is_null() || length == 0 { return Ok(Vec::new()); }
    Ok(unsafe { std::slice::from_raw_parts(data, length as usize) }.to_vec())
}

#[cfg(test)]
fn vaapi_packet(engine: &SafeVaapiEngine, idr: &AtomicBool) -> Result<Vec<u8>, String> {
    let mut data = std::ptr::null();
    let mut length = 0;
    let force_idr = libc::c_int::from(idr.swap(false, Ordering::AcqRel));
    let result = unsafe { vaapi_encode_frame(engine.0, force_idr, &mut data, &mut length) };
    if result != 0 { return Err(format!("VAAPI encode failed: {result}")); }
    if data.is_null() || length == 0 { return Ok(Vec::new()); }
    Ok(unsafe { std::slice::from_raw_parts(data, length as usize) }.to_vec())
}

fn next_portal_frame_deadline(slot: &mut Instant, started: Instant, interval: Duration) -> Instant {
    let next = *slot + interval;
    // After a missed period, start fresh. Otherwise retain the nominal slots so an occasional
    // late wake can be recovered over several frames, at most 500 us per interval.
    *slot = if started >= next { started + interval } else { next };
    (*slot).max(started + interval.saturating_sub(Duration::from_micros(500)))
}

fn run_portal_vaapi(
    config: &Config, portal: &crate::portal::PortalSession, mut engine: Option<SafeVaapiEngine>,
    live_rate: Option<&Arc<crate::video::RateHandle>>,
    units: mpsc::Sender<EncodedUnit>, stop: Arc<AtomicBool>, idr: Arc<AtomicBool>, budget: Arc<Semaphore>,
    applied_rate: &mut Option<crate::video::Rate>, applied_revision: &mut u64,
) -> Result<(), String> {
    use crate::portal::DmaCapture;
    enum DmaStep { Skip, Keepalive, Fresh, Fatal }
    let interval = Duration::from_nanos(1_000_000_000 / u64::from(config.fps));
    // Event driven: the compositor's next frame wakes this loop, and `interval` only limits how
    // often an encode may start. The fixed grid this replaced slept to the next grid slot after
    // being woken by a frame that had already arrived, adding 0-13.3 ms (6.7 ms average at 75 fps)
    // between capture and encode, and beat against the compositor's own refresh.
    let mut seen = 0u64;
    let mut nominal_slot = Instant::now();
    let mut next_allowed = nominal_slot;
    // Floor for a pass that produced nothing (an unchanged buffer, a pending DMA-BUF negotiation):
    // without it a stream that owes a keepalive would re-run forever at full speed.
    let mut next_probe = Instant::now();
    let mut last_send = Instant::now();
    let mut available: Option<Instant> = None;
    let mut stats = CaptureStats::default();
    let mut seed = 1;
    let mut keepalive = random_keepalive(&mut seed);
    let mut dma_version = None;
    let mut cpu_version = None;
    let mut cpu_frame = Vec::new();
    let mut uploaded = false;
    let mut input_dma = false;
    let mut nv12_ready = false;
    let mut blank_frame = false;
    let mut dma_disabled = std::env::var("UU_PORTAL_DMABUF").is_ok_and(|v| v == "0");
    if dma_disabled { log::info!("Portal VAAPI DMA-BUF disabled by setting; pipeline=host-upload"); }
    loop {
        if stop.load(Ordering::Relaxed) || units.is_closed() { return Ok(()); }
        let Some(ref active_engine) = engine else { return Err("VAAPI engine unavailable".into()); };
        sync_rate(active_engine, live_rate, applied_rate, applied_revision);
        let now = Instant::now();
        // Encode when a frame is waiting and the rate gate is open, or when the stream owes a
        // keepalive. Otherwise sleep until one of those: the level-triggered wait wakes on the
        // compositor's next frame, including one that arrived while the previous frame was encoded.
        let keepalive_at = if idr.load(Ordering::Acquire) { now } else { last_send + keepalive };
        // A waiting frame only has to respect the rate gate; an owed keepalive also respects the
        // probe floor, which is what stops an unchanged or negotiating source from spinning.
        let deadline = if available.is_some() { next_allowed }
            else { next_allowed.max(next_probe).max(keepalive_at) };
        if now < deadline {
            let wait = deadline - now;
            match portal.wait_frame(&mut seen, wait) {
                Ok(true) => { available = Some(Instant::now()); continue; }
                Ok(false) => continue,
                // Stream closed or stalled: back off, then probe below so with_dmabuf and
                // capture_changed run the existing lock/scaling recovery instead of aborting.
                Err(_) => std::thread::sleep(wait),
            }
        }
        next_probe = Instant::now() + interval;
        stats.gate(available.take());
        if portal.size() != config.input { return Err("Portal size changed; restart encoder".into()); }
        let permit_started = Instant::now();
        let Some(permit) = acquire(&budget, &units) else { return Ok(()); };
        stats.permit(permit_started);
        if stop.load(Ordering::Relaxed) { return Ok(()); }
        let frame_started = Instant::now();
        let due = last_send.elapsed() >= keepalive || idr.load(Ordering::Acquire);
        // Only the VPP copy (RGB DMA-BUF -> private NV12 surface) runs under the PipeWire lease:
        // once VPP completes and clears the external surface wrapper, the lease is released before
        // HEVC/H.264 encoding begins. That both cuts PipeWire loop lock hold time ~3x and isolates
        // encoder vaSyncBuffer timeouts from compositor buffer recycling. If VPP times out, C waits
        // for GPU completion/reset before returning -12; if still unsignaled (-13), `abandon` tells
        // the lease not to queue the poisoned buffer back to the compositor.
        let mut fatal: Option<(String, Option<u64>)> = None;
        let mut leased_handle: Option<u64> = None;
        let captured = if dma_disabled { Ok(DmaCapture::Cpu) } else {
            portal.with_dmabuf_controlled(Some(config.input), &mut dma_version, |frame, changed, handle_id, abandon| {
                leased_handle = Some(handle_id);
                if !changed && input_dma && nv12_ready {
                    return Ok(if due { DmaStep::Keepalive } else { DmaStep::Skip });
                }
                let e = engine.as_ref().ok_or_else(|| "VAAPI engine dropped".to_string())?;
                if unsafe { vaapi_supports_bounded_sync(e.0) } == 0 {
                    nv12_ready = false;
                    leased_handle = None;
                    return Err("VAAPI driver lacks bounded surface sync for DMA-BUF".into());
                }
                let result = unsafe { vaapi_import_frame(e.0, frame) };
                if result != 0 {
                    nv12_ready = false;
                    return Err(format!("VAAPI DMA-BUF import failed: {result}"));
                }
                let vpp_started = Instant::now();
                let conv = unsafe { vaapi_convert_input(e.0, 1) };
                if conv == -14 {
                    nv12_ready = false;
                    leased_handle = None;
                    return Err("VAAPI driver lacks bounded surface sync for DMA-BUF".into());
                }
                if conv != 0 {
                    nv12_ready = false;
                    if conv == -13 {
                        *abandon = true;
                    }
                    // Do NOT destroy `engine` here: driver surface destruction can itself wait on
                    // the GPU. Return first so `Lease::drop` isolates/unlocks PipeWire, then tear
                    // down `engine` outside the lease below.
                    let scope = if matches!(conv, -12 | -13) { None } else { Some(handle_id) };
                    fatal = Some((format!("VAAPI VPP convert failed: {conv}"), scope));
                    return Ok(DmaStep::Fatal);
                }
                stats.vpp(vpp_started);
                if !input_dma {
                    log::info!("Portal VAAPI capture pipeline=dma-buf zero_cpu_copy=true source={}x{} fourcc={} modifier={}",
                        frame.width, frame.height, frame.fourcc, frame.modifier);
                }
                input_dma = true;
                nv12_ready = true;
                blank_frame = false;
                uploaded = true;
                Ok(DmaStep::Fresh)
            })
        };
        if let Some((error, scope)) = fatal {
            let _ = portal.disable_dmabuf_for(scope);
            drop(engine.take());
            return Err(error);
        }
        let captured = match captured {
            Ok(DmaCapture::SizeChanged) => return Err("Portal size changed; restart encoder".into()),
            Ok(DmaCapture::Unavailable(code, scope)) => {
                if !portal.disable_dmabuf_for(scope) {
                    dma_version = None;
                    continue;
                }
                let reason = match code {
                    14 => "buffer layout unsupported or DMA-BUF disabled for this session",
                    16 => "compositor selected shared memory",
                    17 => "DMA-BUF negotiation or first frame timed out",
                    18 => "explicit synchronization unsupported",
                    19 => "nonlinear modifier unsupported",
                    _ => "DMA-BUF interface unavailable",
                };
                log::warn!("Portal DMA-BUF unavailable: {reason} (code={code}); falling back to native VAAPI host upload");
                dma_disabled = true; nv12_ready = false;
                DmaCapture::Cpu
            },
            Err(error) => {
                if !portal.disable_dmabuf_for(leased_handle) {
                    dma_version = None;
                    continue;
                }
                log::warn!("Portal DMA-BUF unavailable: {error}; falling back to native VAAPI host upload");
                dma_disabled = true; nv12_ready = false;
                DmaCapture::Cpu
            },
            Ok(value) => value,
        };
        let Some(ref active_engine) = engine else { return Err("VAAPI engine unavailable".into()); };
        let (data, encode_started) = match captured {
            DmaCapture::Frame(DmaStep::Fresh) => {
                let enc_started = Instant::now();
                (vaapi_encode_nv12_packet(active_engine, &idr, true)?, enc_started)
            }
            DmaCapture::Frame(DmaStep::Keepalive) => {
                stats.vpp_skipped();
                let enc_started = Instant::now();
                (vaapi_encode_nv12_packet(active_engine, &idr, false)?, enc_started)
            }
            DmaCapture::Frame(DmaStep::Skip | DmaStep::Fatal) | DmaCapture::Pending => continue,
            state @ (DmaCapture::Cpu | DmaCapture::Dead | DmaCapture::Blank) => {
                cpu_frame.resize(config.input.0 as usize * config.input.1 as usize * 4, 0);
                let changed = if matches!(state, DmaCapture::Blank) {
                    let changed = !blank_frame;
                    if changed { cpu_frame.fill(0); }
                    blank_frame = true;
                    cpu_version = None;
                    dma_version = None;
                    changed
                } else if matches!(state, DmaCapture::Dead) {
                    blank_frame = false;
                    let Some(changed) = portal.capture_dead_or_retry(&mut cpu_frame, &mut cpu_version)? else {
                        dma_version = None;
                        continue;
                    };
                    changed
                } else {
                    blank_frame = false;
                    portal.capture_changed(&mut cpu_frame, &mut cpu_version)?
                };
                let fresh = changed || input_dma || !uploaded || !nv12_ready;
                if !fresh && !due { continue; }
                if fresh {
                    let vpp_started = Instant::now();
                    let result = unsafe { vaapi_upload_host_buffer(active_engine.0, cpu_frame.as_ptr(), config.input.0 * 4) };
                    if result != 0 { return Err(format!("Portal VAAPI upload failed: {result}")); }
                    let conv = unsafe { vaapi_convert_input(active_engine.0, 0) };
                    if conv != 0 { return Err(format!("Portal VAAPI VPP convert failed: {conv}")); }
                    stats.vpp(vpp_started);
                    input_dma = false; uploaded = true; nv12_ready = true;
                } else {
                    stats.vpp_skipped();
                }
                let enc_started = Instant::now();
                (vaapi_encode_nv12_packet(active_engine, &idr, fresh)?, enc_started)
            },
            DmaCapture::Unavailable(_, _) | DmaCapture::SizeChanged => unreachable!(),
        };
        if data.is_empty() { continue; }
        stats.encoded(encode_started);
        // VPP and encoding use time within the current frame period.
        next_allowed = next_portal_frame_deadline(&mut nominal_slot, frame_started, interval);
        if units.blocking_send(EncodedUnit { data, permit, completed_at: Instant::now(), rate_epoch: *applied_revision }).is_err() { return Ok(()); }
        last_send = Instant::now();
        keepalive = random_keepalive(&mut seed);
    }
}

fn vaapi_wait(dri3: &Option<SafeDri3Capture>, capture: &mut Option<DamageCapture>, timeout: Duration) -> Result<bool, String> {
    match (dri3, capture) {
        (Some(c), _) => match unsafe { dri3_capture_wait_damage(c.0, timeout.as_millis().min(1000) as i32) } {
            r if r < 0 => Err("XDamage wait on the DRI3 connection failed".into()),
            r => Ok(r > 0),
        },
        (None, Some(c)) => c.wait(timeout),
        (None, None) => unreachable!(),
    }
}

fn run_nvenc(
    config: Config,
    units: mpsc::Sender<EncodedUnit>,
    stop: Arc<AtomicBool>,
    idr: Arc<AtomicBool>,
    budget: Arc<Semaphore>,
) -> Result<(), String> {
    let (width, height) = config.output.unwrap_or(config.input);
    let (width, height) = (width + width % 2, height + height % 2);
    let is_hevc = if config.codec == crate::capture_setting::VideoCodec::H265 {
        1
    } else {
        0
    };

    let chroma_444 = libc::c_int::from(config.chroma == crate::capture_setting::CHROMA_444);
    log::info!(
        "Initializing Native In-Process NVENC: {}x{}, {} fps, GOP {}, CQ {}, HEVC={}, 4:4:4={}",
        width,
        height,
        config.fps,
        config.gop,
        config.cq,
        is_hevc == 1,
        chroma_444 == 1
    );

    let ffi_cfg = NvencConfigFFI {
        width,
        height,
        fps: config.fps,
        gop: config.gop,
        cq: config.cq,
        target_bps: config.target_bps,
        max_bps: config.max_bps,
        buffer_bits: config.buffer_bits,
        is_hevc,
        chroma_444,
    };

    let mut capture = DamageCapture::open(&config.display)
        .map_err(|e| format!("DamageCapture::open failed on {}: {e}", config.display))?;

    let raw_engine = unsafe { nvenc_create(&ffi_cfg) };
    if raw_engine.is_null() {
        return Err("Failed to create native NVENC engine".into());
    }
    let engine = SafeNvencEngine(raw_engine);
    let (cap_width, cap_height) = capture.size();

    // Check if capture buffer can be Zero-Copy registered directly
    let mut zero_copy = false;
    if cap_width == width && cap_height == height {
        let shm_ptr = capture.shm_raw_ptr() as *mut libc::c_void;
        let shm_len = capture.shm_size();
        let res = unsafe { nvenc_register_host_buffer(engine.0, shm_ptr, shm_len) };
        if res == 0 {
            zero_copy = true;
            log::info!("Native NVENC reads the registered XShm buffer directly (no host-to-device copy)");
        } else {
            log::warn!("nvenc_register_host_buffer returned {res}, copying frames with cuMemcpyHtoD");
        }
    } else {
        log::info!(
            "Capture size ({}x{}) != encoder size ({}x{}): {} on the CPU, then cuMemcpyHtoD",
            cap_width,
            cap_height,
            width,
            height,
            if config.output.is_some() { "bilinear scaling" } else { "padding" }
        );
    }

    let bytes_per_line = cap_width as usize * 4;
    let frame_interval = Duration::from_nanos(1_000_000_000 / u64::from(config.fps));

    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| d.subsec_nanos())
        | 1;
    let mut keepalive = random_keepalive(&mut seed);
    let mut dirty = true;
    let mut next_tick = Instant::now();
    let mut last_send = Instant::now();
    let mut permits = VecDeque::with_capacity(4);
    let mut frame_buf = Vec::new();

    let locked = crate::session_lock::watch_locked();
    let mut last_locked = *locked.borrow();

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        let deadline = if dirty {
            next_tick
        } else {
            last_send + keepalive
        };
        let wait = deadline.saturating_duration_since(Instant::now());

        if capture.wait(wait.max(Duration::from_millis(1)))? {
            dirty = true;
        }

        let now = Instant::now();
        if dirty && now < next_tick {
            continue;
        }
        if !dirty && now < last_send + keepalive {
            continue;
        }

        let Some(permit) = acquire(&budget, &units) else {
            return Ok(());
        };
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        dirty |= capture.wait(Duration::ZERO)?;
        let state = *locked.borrow();
        if state != last_locked {
            dirty = true;
            last_locked = state;
        }

        let force_idr = if idr.swap(false, Ordering::AcqRel) {
            1
        } else {
            0
        };

        let mut out_data: *const u8 = std::ptr::null();
        let mut out_len: u32 = 0;

        let res = if zero_copy {
            // In Zero-Copy mode: refresh XShm, then NVENC DMA-reads directly from shm_ptr
            if dirty {
                capture.refresh()?;
            }
            unsafe {
                nvenc_encode_frame(
                    engine.0,
                    std::ptr::null(), // NULL means use zero-copy registered buffer
                    bytes_per_line as u32,
                    force_idr,
                    &mut out_data,
                    &mut out_len,
                )
            }
        } else {
            // Copy path: 其他分辨率 is scaled; an odd native size is padded with black to the even encoder size
            let source = capture.capture()?;
            let (target_ptr, pitch) = if cap_width == width && cap_height == height {
                (source.as_ptr(), bytes_per_line as u32)
            } else {
                frame_buf.resize(width as usize * height as usize * 4, 0);
                if config.output.is_some() {
                    scale_bilinear_argb32(source, cap_width, cap_height, &mut frame_buf, width, height);
                } else {
                    pad_argb32(source, cap_width, cap_height, &mut frame_buf, width, height);
                }
                (frame_buf.as_ptr(), width * 4)
            };
            unsafe {
                nvenc_encode_frame(
                    engine.0,
                    target_ptr,
                    pitch,
                    force_idr,
                    &mut out_data,
                    &mut out_len,
                )
            }
        };

        if res != 0 {
            return Err(format!("nvenc_encode_frame failed with error {res}"));
        }

        if out_len > 0 && !out_data.is_null() {
            let data = unsafe { std::slice::from_raw_parts(out_data, out_len as usize) }.to_vec();
            permits.push_back(permit);
            let send_permit = permits.pop_front().unwrap();

            if units
                .blocking_send(EncodedUnit {
                    data,
                    permit: send_permit,
                    completed_at: Instant::now(),
                    rate_epoch: config.rate_revision,
                })
                .is_err()
            {
                return Ok(());
            }
        }

        let now = Instant::now();
        if dirty {
            let next = next_tick + frame_interval;
            next_tick = if next > now {
                next
            } else {
                now + frame_interval
            };
            dirty = false;
        }
        last_send = now;
        keepalive = random_keepalive(&mut seed);
    }
}

/// Bilinear ARGB32 scaling (the former swscale path used SWS_BILINEAR), with 8-bit fixed-point
/// weights and pixel centers aligned.
fn scale_bilinear_argb32(src: &[u8], src_w: u32, src_h: u32, dst: &mut [u8], dst_w: u32, dst_h: u32) {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return;
    }
    // Source position in 1/256 pixel units: (d + 0.5) * src / dst - 0.5, clamped to the image.
    let taps = |dst_len: u32, src_len: u32| -> Vec<(usize, usize, u32)> {
        (0..dst_len)
            .map(|d| {
                let pos = ((2 * u64::from(d) + 1) * u64::from(src_len) * 128 / u64::from(dst_len)).saturating_sub(128);
                let first = ((pos >> 8) as usize).min(src_len as usize - 1);
                let second = (first + 1).min(src_len as usize - 1);
                (first, second, (pos & 255) as u32)
            })
            .collect()
    };
    let (columns, rows) = (taps(dst_w, src_w), taps(dst_h, src_h));
    let src_stride = src_w as usize * 4;
    for (dst_row, &(y0, y1, fy)) in dst.chunks_exact_mut(dst_w as usize * 4).zip(&rows) {
        let (top, bottom) = (&src[y0 * src_stride..], &src[y1 * src_stride..]);
        for (pixel, &(x0, x1, fx)) in dst_row.chunks_exact_mut(4).zip(&columns) {
            for c in 0..4 {
                let lerp = |row: &[u8]| u32::from(row[x0 * 4 + c]) * (256 - fx) + u32::from(row[x1 * 4 + c]) * fx;
                pixel[c] = ((lerp(top) * (256 - fy) + lerp(bottom) * fy + 32768) >> 16) as u8;
            }
        }
    }
}

/// Copies the capture into the top-left corner of the larger encoder frame; the rest stays black.
fn pad_argb32(src: &[u8], src_w: u32, src_h: u32, dst: &mut [u8], dst_w: u32, dst_h: u32) {
    let (src_stride, dst_stride) = (src_w as usize * 4, dst_w as usize * 4);
    let copy = src_stride.min(dst_stride);
    for (y, dst_row) in dst.chunks_exact_mut(dst_stride).enumerate() {
        if y < src_h.min(dst_h) as usize {
            dst_row[..copy].copy_from_slice(&src[y * src_stride..y * src_stride + copy]);
            dst_row[copy..].fill(0);
        } else {
            dst_row.fill(0);
        }
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[test]
    fn portal_rate_gate_does_not_accumulate_timer_lateness() {
        let start = Instant::now();
        for fps in [5, 60, 120, 144] {
            let interval = Duration::from_nanos(1_000_000_000 / fps);
            let mut slot = start;
            let mut next = start;
            for frame in 1..=600 {
                // A frame is ready at each deadline, but the timed wait returns 140 us late.
                let woke = next + Duration::from_micros(140);
                next = next_portal_frame_deadline(&mut slot, woke, interval);
                assert_eq!(next, start + interval * frame);
            }
        }
    }

    #[test]
    fn portal_rate_gate_bounds_catch_up_and_restarts_after_a_stall() {
        let due = Instant::now();
        let interval = Duration::from_nanos(1_000_000_000 / 120);
        let woke = due + Duration::from_millis(3);
        let mut slot = due;
        let mut next = woke;
        for _ in 0..6 {
            let previous_start = next;
            next = next_portal_frame_deadline(&mut slot, previous_start, interval);
            assert!(next >= previous_start + interval - Duration::from_micros(500));
        }
        assert_eq!(next, due + interval * 6, "recover the original phase over several frames");
        let resumed = next + Duration::from_secs(1);
        assert_eq!(next_portal_frame_deadline(&mut slot, resumed, interval), resumed + interval);
    }

    #[test]
    #[ignore = "requires a VAAPI render node and FFmpeg for independent decode verification"]
    fn vaapi_locked_frames_decode_as_black_in_both_codecs() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        for codec in [crate::capture_setting::VideoCodec::H264, crate::capture_setting::VideoCodec::H265] {
            let frame = black_frame(codec).unwrap();
            let mut decoder = Command::new("ffmpeg").args(["-hide_banner", "-loglevel", "error", "-i", "pipe:0",
                "-frames:v", "1", "-vf", "scale=1:1", "-pix_fmt", "rgb24", "-f", "rawvideo", "pipe:1"])
                .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
            decoder.stdin.take().unwrap().write_all(&frame).unwrap();
            let decoded = decoder.wait_with_output().unwrap();
            assert!(decoded.status.success(), "decode failed for {codec:?}");
            assert_eq!(decoded.stdout.len(), 3);
            assert!(decoded.stdout.iter().all(|&value| value <= 3), "frame is not black for {codec:?}");
        }
    }

    #[test]
    #[ignore = "requires a VAAPI render node (EncSlice or Intel EncSliceLP) and FFmpeg"]
    fn vaapi_multi_gop_and_on_demand_idr_decode_cleanly_in_both_codecs() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let device = CString::new(crate::video::vaapi_device()).unwrap();
        let (w, h) = (1280u32, 720u32);
        for (is_hevc, fmt) in [(0, "h264"), (1, "hevc")] {
            let cfg = VaapiConfigFFI {
                width: w, height: h, cap_width: w, cap_height: h, fps: 60, gop: 20, cq: 18,
                target_bps: 4_000_000, max_bps: 6_000_000, buffer_bits: 1_000_000,
                is_hevc, device: device.as_ptr(),
            };
            let engine = SafeVaapiEngine(unsafe { vaapi_create(&cfg) });
            assert!(!engine.0.is_null(), "vaapi_create failed for {fmt}");
            let mut stream = Vec::new();
            let mut pixels = vec![0u8; (w * h * 4) as usize];
            let mut expected_rgb = Vec::new();
            let total_frames = 30u32;
            for idx in 0..total_frames {
                // High-contrast moving window over a textured grid (exposes inter-prediction MV/reference bugs).
                let bx = (100 + idx * 24) % (w - 320);
                let by = (80 + idx * 14) % (h - 220);
                for y in 0..h {
                    for x in 0..w {
                        let off = ((y * w + x) * 4) as usize;
                        let in_box = x >= bx && x < bx + 300 && y >= by && y < by + 200;
                        let (b, g, r) = if in_box {
                            if (x - bx) < 8 || (y - by) < 28 { (220u8, 120u8, 40u8) }
                            else if ((x - bx) / 20 + (y - by) / 20) % 2 == 0 { (245, 245, 245) }
                            else { (40, 40, 40) }
                        } else if (x / 64 + y / 64) % 2 == 0 {
                            (32, 36, 42)
                        } else {
                            (58, 64, 72)
                        };
                        pixels[off] = b;
                        pixels[off + 1] = g;
                        pixels[off + 2] = r;
                        pixels[off + 3] = 255;
                    }
                }
                // Downsample 16x16 grid of reference pixels (BGR -> RGB) to check decoded fidelity per frame.
                let mut ref_sample = Vec::with_capacity((w / 16 * h / 16 * 3) as usize);
                for y in (0..h).step_by(16) {
                    for x in (0..w).step_by(16) {
                        let off = ((y * w + x) * 4) as usize;
                        ref_sample.push(pixels[off + 2]);
                        ref_sample.push(pixels[off + 1]);
                        ref_sample.push(pixels[off]);
                    }
                }
                expected_rgb.push(ref_sample);
                assert_eq!(unsafe { vaapi_upload_host_buffer(engine.0, pixels.as_ptr(), w * 4) }, 0);
                let mut out_ptr = std::ptr::null();
                let mut out_len = 0u32;
                let force_idr = i32::from(idx == 11);
                assert_eq!(unsafe { vaapi_encode_frame(engine.0, force_idr, &mut out_ptr, &mut out_len) }, 0, "encode failed at {idx} for {fmt}");
                assert!(!out_ptr.is_null() && out_len > 0);
                let packet = unsafe { std::slice::from_raw_parts(out_ptr, out_len as usize) };
                stream.extend_from_slice(packet);
            }
            let mut decoder = Command::new("ffmpeg").args([
                "-hide_banner", "-loglevel", "error", "-f", fmt, "-i", "pipe:0",
                "-fps_mode", "passthrough", "-pix_fmt", "rgb24", "-f", "rawvideo", "pipe:1",
            ]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
            let mut stdin = decoder.stdin.take().unwrap();
            let writer = std::thread::spawn(move || stdin.write_all(&stream));
            let out = decoder.wait_with_output().unwrap();
            writer.join().unwrap().unwrap();
            assert!(out.status.success() && out.stderr.is_empty(),
                "{fmt} decode errors: {}", String::from_utf8_lossy(&out.stderr));
            let frame_bytes = (w * h * 3) as usize;
            assert_eq!(out.stdout.len(), (total_frames as usize) * frame_bytes,
                "{fmt} decoded frame count mismatch");
            let mut worst_mae = 0.0f64;
            for (idx, ref_sample) in expected_rgb.iter().enumerate() {
                let dec = &out.stdout[idx * frame_bytes..(idx + 1) * frame_bytes];
                let mut sum_err = 0u64;
                let mut s = 0usize;
                for y in (0..h).step_by(16) {
                    for x in (0..w).step_by(16) {
                        let off = ((y * w + x) * 3) as usize;
                        sum_err += u64::from(dec[off].abs_diff(ref_sample[s]));
                        sum_err += u64::from(dec[off + 1].abs_diff(ref_sample[s + 1]));
                        sum_err += u64::from(dec[off + 2].abs_diff(ref_sample[s + 2]));
                        s += 3;
                    }
                }
                let mae = sum_err as f64 / ref_sample.len() as f64;
                worst_mae = worst_mae.max(mae);
                assert!(mae < 12.0, "{fmt} frame {idx} smeared/corrupted: MAE={mae:.2}");
            }
            println!("{fmt} moving-window fidelity passed: frames={total_frames} worst_mae={worst_mae:.2}");
        }
    }

    #[tokio::test]
    async fn closing_encoder_output_wakes_a_blocked_capture() {
        let budget = Arc::new(Semaphore::new(0));
        let (tx, rx) = mpsc::channel(1);
        let worker = tokio::task::spawn_blocking(move || acquire(&budget, &tx));
        tokio::task::yield_now().await;
        drop(rx);
        assert!(tokio::time::timeout(Duration::from_secs(1), worker).await.unwrap().unwrap().is_none());
    }

    #[tokio::test]
    async fn stream_slot_waits_for_the_previous_encoder_of_the_same_stream() {
        let (stream, other) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let held = stream_slot(&stream).await;
        // Another stream is independent.
        drop(tokio::time::timeout(Duration::from_secs(1), stream_slot(&other)).await.unwrap());
        assert!(tokio::time::timeout(Duration::from_millis(50), stream_slot(&stream)).await.is_err());
        drop(held);
        drop(tokio::time::timeout(Duration::from_secs(1), stream_slot(&stream)).await.unwrap());
    }
}

#[cfg(test)]
mod frame_tests {
    use super::*;
    fn image(w: u32, h: u32, pixel: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
        let pixel = &pixel;
        (0..h).flat_map(|y| (0..w).flat_map(move |x| pixel(x, y))).collect()
    }

    #[test]
    fn bilinear_scaling_keeps_flat_areas_and_averages_edges() {
        let flat = image(8, 6, |_, _| [10, 20, 30, 255]);
        let mut out = vec![0; 4 * 3 * 4];
        scale_bilinear_argb32(&flat, 8, 6, &mut out, 4, 3);
        assert!(out.chunks(4).all(|p| p == [10, 20, 30, 255]));
        // A 2:1 downscale of alternating black/white columns lands between the two, not on one of them.
        let stripes = image(4, 2, |x, _| if x % 2 == 0 { [0; 4] } else { [255; 4] });
        let mut out = vec![0; 2 * 1 * 4];
        scale_bilinear_argb32(&stripes, 4, 2, &mut out, 2, 1);
        assert!(out.iter().all(|&v| v == 128), "{out:?}");
        // Upscaling interpolates and never reads past the last row/column.
        let ramp = image(2, 1, |x, _| [(x * 200) as u8; 4]);
        let mut out = vec![0; 4 * 2 * 4];
        scale_bilinear_argb32(&ramp, 2, 1, &mut out, 4, 2);
        let row: Vec<u8> = out[..16].chunks(4).map(|p| p[0]).collect();
        assert_eq!(row, vec![0, 50, 150, 200]);
    }

    #[test]
    fn odd_capture_is_padded_with_black_not_stretched() {
        let src = image(3, 3, |x, y| [(x + 3 * y + 1) as u8; 4]);
        let mut out = vec![9; 4 * 4 * 4];
        pad_argb32(&src, 3, 3, &mut out, 4, 4);
        for y in 0..4usize {
            for x in 0..4usize {
                let expected = if x < 3 && y < 3 { (x + 3 * y + 1) as u8 } else { 0 };
                assert_eq!(out[(y * 4 + x) * 4], expected, "({x},{y})");
            }
        }
    }
}

#[cfg(all(test, target_os = "linux", feature = "portal"))]
mod portal_hardware_tests {
    // Run on an unlocked desktop with saved Portal consent and VAAPI selected.
    // FFmpeg is used only for independent decode verification; desktop contents
    // are neither saved nor printed. These tests do not lock the real desktop
    // or replace system lock-screen acceptance in an isolated VM.
    use super::*;
    use crate::capture_setting::VideoCodec;

    struct Runner {
        stop: Arc<AtomicBool>,
        idr: Arc<AtomicBool>,
        rx: mpsc::Receiver<EncodedUnit>,
        job: Option<tokio::task::JoinHandle<Result<(), String>>>,
    }
    impl Drop for Runner {
        fn drop(&mut self) { self.stop.store(true, Ordering::Release); self.rx.close(); }
    }
    impl Runner {
        fn start(portal: Arc<crate::portal::PortalSession>, codec: VideoCodec) -> Self {
            let config = Config {
                input: portal.size(), portal: Some(portal), display: "wayland:portal".into(),
                output: None, fps: 60, gop: 120, cq: 18, target_bps: 16_000_000,
                max_bps: 30_000_000, buffer_bits: 500_000, codec,
                chroma: crate::capture_setting::CHROMA_420, vaapi: true, rate_revision: 0, rate: None,
            };
            let (tx, rx) = mpsc::channel(4);
            let stop = Arc::new(AtomicBool::new(false));
            let idr = Arc::new(AtomicBool::new(false));
            let worker_stop = stop.clone();
            let worker_idr = idr.clone();
            let job = tokio::task::spawn_blocking(move || run(config, tx, worker_stop, worker_idr, Arc::new(Semaphore::new(4))));
            Self { stop, idr, rx, job: Some(job) }
        }
        async fn frame(&mut self) -> Vec<u8> {
            tokio::time::timeout(Duration::from_secs(10), self.rx.recv()).await.unwrap()
                .expect("native capture stopped").data
        }
        async fn hevc_idr(&mut self) -> Vec<u8> {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let frame = self.frame().await;
                    let nals = crate::hevc::payload(usize::MAX, &frame).unwrap();
                    if nals.iter().any(|nal| (16..=21).contains(&((nal[0] >> 1) & 63))) { return frame; }
                }
            }).await.expect("requested HEVC IDR did not arrive")
        }
        async fn finish(mut self) {
            self.stop.store(true, Ordering::Release);
            self.rx.close();
            tokio::time::timeout(Duration::from_secs(2), self.job.take().unwrap()).await
                .expect("native encoder did not stop").unwrap().unwrap();
        }
    }
    fn decoded_is_black(data: &[u8], codec: VideoCodec) -> bool {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let format = if codec == VideoCodec::H265 { "hevc" } else { "h264" };
        let mut decoder = Command::new("ffmpeg").args(["-hide_banner", "-loglevel", "error", "-f", format,
            "-i", "pipe:0", "-frames:v", "1", "-pix_fmt", "rgb24", "-f", "rawvideo", "pipe:1"])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        // Concurrent writing avoids filling stdin while the decoder is blocked on raw RGB output.
        let mut input = decoder.stdin.take().unwrap();
        let encoded = data.to_vec();
        let writer = std::thread::spawn(move || input.write_all(&encoded));
        let decoded = decoder.wait_with_output().unwrap();
        writer.join().unwrap().unwrap();
        assert!(decoded.status.success() && !decoded.stdout.is_empty(), "native frame decode failed");
        decoded.stdout.iter().all(|&value| value <= 8)
    }
    fn resources() -> (usize, u64) {
        let fds = std::fs::read_dir("/proc/self/fd").unwrap().count();
        let rss = std::fs::read_to_string("/proc/self/status").unwrap().lines()
            .find(|line| line.starts_with("VmRSS:")).unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
        (fds, rss)
    }
    async fn reopen(portal: Arc<crate::portal::PortalSession>) {
        tokio::task::spawn_blocking(move || {
            portal.end();
            portal.begin();
            assert!(portal.recover(&AtomicBool::new(false)).unwrap());
        }).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires an unlocked Portal desktop and saved consent"]
    async fn portal_snapshot_after_dma_initialization() {
        let _ = env_logger::builder().is_test(true).try_init();
        tokio::task::spawn_blocking(|| {
            let mut portal = crate::portal::PortalSession::open().unwrap();
            let (locked, watch) = tokio::sync::watch::channel(Some(false));
            portal.test_lock_watch(watch);
            assert!(matches!(portal.with_dmabuf(&mut None, |_, _| Ok(())).unwrap(), crate::portal::DmaCapture::Frame(())));
            let (width, height) = portal.size();
            let mut pixels = vec![0; width as usize * height as usize * 4];
            for state in [Some(true), None, Some(false)] {
                locked.send(state).unwrap();
                portal.pause();
                portal.capture(&mut pixels).unwrap();
                assert!(pixels.chunks_exact(4).any(|p| p[..3].iter().any(|&v| v > 8)),
                    "healthy snapshot was blanked for lock state {state:?}");
                println!("snapshot_lock_state={state:?} valid_frame=true");
            }
        }).await.unwrap();
    }

    // RUST_LOG=info cargo test --release --locked --features wayland,portal,inproc-av portal_five_minute_endurance_with_full_decode -- --ignored --nocapture
    // Run for five minutes, independently decode every output frame, require equal
    // encoded/decoded counts and no decode errors, and report CPU, RSS and FDs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "five-minute real Portal/VAAPI run; FFmpeg is used only as an independent decoder"]
    async fn portal_five_minute_endurance_with_full_decode() {
        use std::process::Stdio;
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let _ = env_logger::builder().is_test(true).try_init();
        let session = tokio::task::spawn_blocking(crate::portal::PortalSession::open).await.unwrap().unwrap();
        let mut runner = Runner::start(Arc::new(session), VideoCodec::H265);
        let mut decoder = tokio::process::Command::new("ffmpeg").args([
            "-hide_banner", "-loglevel", "error", "-nostats", "-threads", "2", "-f", "hevc", "-i", "pipe:0",
            "-progress", "pipe:1", "-fps_mode", "passthrough", "-f", "null", "/dev/null",
        ]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).spawn().unwrap();
        let mut input = decoder.stdin.take().unwrap();
        let mut errors = decoder.stderr.take().unwrap();
        let error_reader = tokio::spawn(async move {
            let mut total = 0usize;
            let mut buffer = [0; 2048];
            loop {
                let count = errors.read(&mut buffer).await.unwrap();
                if count == 0 { return total; }
                total += count;
            }
        });
        let output = decoder.stdout.take().unwrap();
        let progress_reader = tokio::spawn(async move {
            let mut lines = BufReader::new(output).lines();
            let mut frames = 0u64;
            while let Some(line) = lines.next_line().await.unwrap() {
                if let Some(count) = line.strip_prefix("frame=") { frames = count.trim().parse().unwrap(); }
            }
            frames
        });
        let ticks = || {
            let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
            let fields: Vec<_> = stat.split_once(") ").unwrap().1.split_whitespace().collect();
            fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
        };
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
        let started = Instant::now();
        let mut last_report = started;
        let mut last_ticks = ticks();
        let mut frames = 0u64;
        while started.elapsed() < Duration::from_secs(300) {
            let data = runner.frame().await;
            tokio::time::timeout(Duration::from_secs(5), input.write_all(&data)).await
                .expect("verification decoder stopped consuming").unwrap();
            frames += 1;
            if last_report.elapsed() >= Duration::from_secs(30) {
                let now_ticks = ticks();
                let cpu = (now_ticks - last_ticks) as f64 / hz / last_report.elapsed().as_secs_f64() * 100.0;
                let (fds, rss_kb) = resources();
                println!("endurance elapsed_s={} encoded_frames={} cpu_percent={cpu:.2} fds={fds} rss_kb={rss_kb}",
                    started.elapsed().as_secs(), frames);
                last_ticks = now_ticks; last_report = Instant::now();
            }
        }
        runner.finish().await;
        drop(input);
        assert!(tokio::time::timeout(Duration::from_secs(15), decoder.wait()).await.unwrap().unwrap().success());
        assert_eq!(error_reader.await.unwrap(), 0, "verification decoder reported errors");
        let decoded = progress_reader.await.unwrap();
        assert_eq!(decoded, frames, "decoder lost encoded frames");
        println!("portal_endurance_passed duration_s={:.2} encoded_frames={} decoded_frames={} decode_errors=0",
            started.elapsed().as_secs_f64(), frames, decoded);
    }

    // RUST_LOG=info cargo test --release --locked --features wayland,portal,inproc-av portal_restart_resources_and_injected_lock_recovery -- --ignored --nocapture
    // Recreate the Portal session 20 times, check resource growth, then inject
    // locked/unknown/unlocked observations to check healthy-frame passthrough.
    // Actually close/restore the source to check placeholder black and recovery.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires an unlocked Portal desktop, saved consent, VAAPI and FFmpeg decode verification"]
    async fn portal_restart_resources_and_injected_lock_recovery() {
        let _ = env_logger::builder().is_test(true).try_init();
        let mut session = tokio::task::spawn_blocking(crate::portal::PortalSession::open).await.unwrap().unwrap();
        // Inject only the lock observation; actual session closure/recovery and GPU encoding remain real.
        // This does not lock the user's desktop or bypass the isolated-VM acceptance guard.
        let (locked, watch) = tokio::sync::watch::channel(Some(false));
        session.test_lock_watch(watch);
        let portal = Arc::new(session);
        let mut snapshots = Vec::new();
        for cycle in 0..20 {
            let codec = if cycle % 2 == 0 { VideoCodec::H264 } else { VideoCodec::H265 };
            let mut runner = Runner::start(portal.clone(), codec);
            let frame = runner.frame().await;
            assert!(!decoded_is_black(&frame, codec), "restarted stream stayed black");
            runner.finish().await;
            reopen(portal.clone()).await;
            let (fds, rss_kb) = resources();
            snapshots.push((fds, rss_kb));
            println!("restart_cycle={} fds={} rss_kb={}", cycle + 1, fds, rss_kb);
        }
        let warm = snapshots[3];
        let last = *snapshots.last().unwrap();
        assert!(last.0 <= warm.0 + 2, "file descriptors grew across reconnects");
        assert!(last.1 <= warm.1 + 32 * 1024, "resident memory grew across reconnects");

        let mut runner = Runner::start(portal.clone(), VideoCodec::H265);
        assert!(!decoded_is_black(&runner.hevc_idr().await, VideoCodec::H265));
        for state in [Some(true), None, Some(false)] {
            locked.send(state).unwrap();
            portal.pause();
            runner.idr.store(true, Ordering::Release);
            assert!(!decoded_is_black(&runner.hevc_idr().await, VideoCodec::H265),
                "healthy DMA source was blanked for lock state {state:?}");
            assert!(matches!(portal.with_dmabuf(&mut None, |_, _| Ok(())).unwrap(), crate::portal::DmaCapture::Frame(())));
            println!("dma_lock_state={state:?} valid_frame=true");
        }
        for cycle in 0..3 {
            locked.send(Some(true)).unwrap();
            portal.end();
            runner.idr.store(true, Ordering::Release);
            assert!(decoded_is_black(&runner.hevc_idr().await, VideoCodec::H265), "closed Portal source did not produce placeholder frame");
            assert!(matches!(portal.with_dmabuf(&mut None, |_, _| Ok(())).unwrap(), crate::portal::DmaCapture::Cpu));
            locked.send(Some(false)).unwrap();
            let session = portal.clone();
            tokio::task::spawn_blocking(move || { session.begin(); session.recover(&AtomicBool::new(false)).unwrap(); }).await.unwrap();
            runner.idr.store(true, Ordering::Release);
            assert!(!decoded_is_black(&runner.hevc_idr().await, VideoCodec::H265), "desktop did not recover after unlock observation");
            println!("injected_lock_cycle={} source_closed=true placeholder_black=true same_arc_recovered=true", cycle + 1);
        }
        runner.finish().await;
        println!("portal_restart_resources_passed cycles=20 fd_growth={} rss_growth_kb={} injected_lock_cycles=3",
            last.0 as i64 - warm.0 as i64, last.1 as i64 - warm.1 as i64);
    }
}

#[cfg(test)]
mod rate_tests {
    use super::*;
    use crate::video::{Rate, RateHandle};

    /// Hardware acceptance for a live rate step: noise at 2 Mbit/s, then retune the *running* engine
    /// to 8 Mbit/s. The second window must carry more bits, and only the retune may add a keyframe -
    /// the context, surfaces and imported buffer stay, which is what makes this cheaper than the
    /// pipeline restart it replaces (see `vaapi_set_rate`).
    #[test]
    #[ignore = "needs a VAAPI render node: cargo test --features inproc-av -- --ignored vaapi_live_rate"]
    fn vaapi_live_rate_retune_raises_the_bitrate_with_one_keyframe() {
        let (width, height) = (1280u32, 720u32);
        let low = Rate { target_bps: 1_500_000, max_bps: 2_000_000, buffer_bits: 250_000, cq: 18 };
        let high = Rate { target_bps: 6_000_000, max_bps: 8_000_000, buffer_bits: 1_000_000, cq: 18 };
        let device = CString::new(crate::video::vaapi_device()).unwrap();
        let ffi = VaapiConfigFFI {
            width, height, cap_width: width, cap_height: height, fps: 60, gop: 250, cq: low.cq,
            target_bps: low.target_bps, max_bps: low.max_bps, buffer_bits: low.buffer_bits,
            is_hevc: 1, device: device.as_ptr(),
        };
        let engine = SafeVaapiEngine(unsafe { vaapi_create(&ffi) });
        assert!(!engine.0.is_null(), "VAAPI engine unavailable on {}", crate::video::vaapi_device());
        assert_eq!(unsafe { vaapi_supports_bounded_sync(engine.0) }, 1);
        let rate = Arc::new(RateHandle::new(low));
        let mut applied = None;
        let mut applied_revision = 0;
        sync_rate(&engine, Some(&rate), &mut applied, &mut applied_revision);
        assert_eq!(applied, Some(low), "the first frame carries the requested rate");
        assert_eq!(unsafe { vaapi_supports_rate_control(engine.0) }, 1);

        let idr = AtomicBool::new(false);
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        let mut seed = 0x1234_5678u32;
        // Equal-rate windows of different lengths would compare total bytes, so keep both counts.
        let (mut low_bytes, mut low_frames, mut high_bytes, mut high_frames) = (0u64, 0u64, 0u64, 0u64);
        let mut irap_frames = 0;
        for frame in 0..=180 {
            if frame == 120 {
                rate.set(high);
                sync_rate(&engine, Some(&rate), &mut applied, &mut applied_revision);
                assert_eq!(applied, Some(high));
                assert_eq!(applied_revision, 1);
            }
            // Full-resolution noise keeps the encoder busy, so a bitrate step is measurable.
            for pixel in pixels.chunks_exact_mut(4) {
                seed ^= seed << 13; seed ^= seed >> 17; seed ^= seed << 5;
                pixel.copy_from_slice(&seed.to_le_bytes());
            }
            assert_eq!(unsafe { vaapi_upload_host_buffer(engine.0, pixels.as_ptr(), width * 4) }, 0);
            let packet = vaapi_packet(&engine, &idr).expect("encode");
            assert!(!packet.is_empty(), "frame {frame} produced no access unit");
            if crate::video::is_independent_frame_for(&packet, crate::capture_setting::VideoCodec::H265) {
                irap_frames += 1;
                assert!(frame == 0 || frame == 120, "only the first frame and the retune are keyframes, not {frame}");
            }
            match frame {
                30..=89 => { low_bytes += packet.len() as u64; low_frames += 1; }
                150..=179 => { high_bytes += packet.len() as u64; high_frames += 1; }
                _ => (),
            }
        }
        assert_eq!(irap_frames, 2, "the first frame and the retune, and nothing else");
        let kbps = |bytes: u64, frames: u64| bytes as f64 * 8.0 * 60.0 / frames as f64 / 1000.0;
        let (low, high) = (kbps(low_bytes, low_frames), kbps(high_bytes, high_frames));
        assert!(high > low * 1.5, "the 8 Mbit/s window should carry more than 1.5x the 2 Mbit/s one: {low:.0} -> {high:.0} kbit/s");
    }

    /// Static keepalive frames (count_gop=0) reuse the prepared NV12 surface without firing
    /// periodic GOP IDRs while still advancing H.264 frame_num / HEVC POC so the entire stream
    /// decodes cleanly; explicit PLI (force_idr=1) and active changed frames (count_gop=1) still
    /// emit IDRs on schedule, and HEVC forces an IDR at POC 4095 before 12-bit POC LSB wrap.
    /// Also checks NULL out_len / out_data handling.
    #[test]
    #[ignore = "needs a VAAPI render node: cargo test --features inproc-av -- --ignored vaapi_keepalive_skips_periodic_idr"]
    fn vaapi_keepalive_skips_periodic_idr_and_answers_pli() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let decode_and_count = |fmt: &str, stream: Vec<u8>, expected: usize| {
            let mut decoder = Command::new("ffmpeg").args([
                "-hide_banner", "-loglevel", "error", "-nostats", "-f", fmt, "-i", "pipe:0",
                "-progress", "pipe:1", "-fps_mode", "passthrough", "-f", "null", "/dev/null",
            ]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
            let mut stdin = decoder.stdin.take().unwrap();
            let writer = std::thread::spawn(move || stdin.write_all(&stream));
            let out = decoder.wait_with_output().unwrap();
            writer.join().unwrap().unwrap();
            assert!(out.status.success() && out.stderr.is_empty(),
                "{fmt} keepalive stream decode errors (expected={expected}): {}", String::from_utf8_lossy(&out.stderr));
            let decoded: usize = String::from_utf8_lossy(&out.stdout).lines()
                .filter_map(|line| line.strip_prefix("frame="))
                .filter_map(|v| v.trim().parse().ok())
                .last()
                .unwrap_or(0);
            assert_eq!(decoded, expected, "{fmt} decoded frame count mismatch");
        };

        let (width, height) = (1280u32, 720u32);
        let device = CString::new(crate::video::vaapi_device()).unwrap();
        let pixels = vec![0x42u8; (width * height * 4) as usize];
        for (is_hevc, codec, fmt) in [
            (0, crate::capture_setting::VideoCodec::H264, "h264"),
            (1, crate::capture_setting::VideoCodec::H265, "hevc"),
        ] {
            let ffi = VaapiConfigFFI {
                width, height, cap_width: width, cap_height: height, fps: 60, gop: 10, cq: 18,
                target_bps: 4_000_000, max_bps: 6_000_000, buffer_bits: 1_000_000,
                is_hevc, device: device.as_ptr(),
            };
            let engine = SafeVaapiEngine(unsafe { vaapi_create(&ffi) });
            assert!(!engine.0.is_null(), "VAAPI engine unavailable on {} ({fmt})", crate::video::vaapi_device());
            assert_eq!(unsafe { vaapi_upload_host_buffer(engine.0, pixels.as_ptr(), width * 4) }, 0);
            assert_eq!(unsafe { vaapi_convert_input(engine.0, 0) }, 0);

            let idr = AtomicBool::new(false);
            let mut stream = Vec::new();
            let mut total_frames = 0usize;
            let first = vaapi_encode_nv12_packet(&engine, &idr, true).expect("frame 0");
            assert!(crate::video::is_independent_frame_for(&first, codec), "{fmt} frame 0 must be IDR");
            stream.extend_from_slice(&first);
            total_frames += 1;

            // 25 unchanged keepalive frames (> 2x GOP and > H.264 4-bit frame_num wrap) must emit
            // zero periodic IDRs and still keep POC / frame_num strictly valid for decoders.
            for i in 0..25 {
                let p = vaapi_encode_nv12_packet(&engine, &idr, false).expect("keepalive");
                assert!(!p.is_empty(), "{fmt} keepalive {i} empty");
                assert!(!crate::video::is_independent_frame_for(&p, codec),
                    "{fmt} keepalive frame {i} must not trigger periodic IDR");
                stream.extend_from_slice(&p);
                total_frames += 1;
            }

            // An explicit PLI/FIR still triggers an immediate IDR even on a keepalive frame.
            idr.store(true, Ordering::Release);
            let pli = vaapi_encode_nv12_packet(&engine, &idr, false).expect("pli");
            assert!(crate::video::is_independent_frame_for(&pli, codec), "{fmt} PLI must produce IDR");
            stream.extend_from_slice(&pli);
            total_frames += 1;

            // After `gop` active changed frames (count_gop=1), periodic IDR fires again.
            let mut saw_periodic = false;
            for _ in 0..10 {
                let f = vaapi_encode_nv12_packet(&engine, &idr, true).expect("active");
                stream.extend_from_slice(&f);
                total_frames += 1;
                if crate::video::is_independent_frame_for(&f, codec) {
                    saw_periodic = true;
                    break;
                }
            }
            assert!(saw_periodic, "{fmt} active changed frames must still emit periodic IDR after GOP");

            decode_and_count(fmt, stream, total_frames);

            // NULL out_len / out_data must not crash.
            assert_eq!(unsafe { vaapi_encode_nv12(engine.0, 0, 0, std::ptr::null_mut(), std::ptr::null_mut()) }, 0);
        }

        // HEVC 12-bit POC LSB wrap boundary (poc_since_idr == 4095) forces one IDR and resets POC cleanly.
        let (sw, sh) = (128u32, 128u32);
        let small_pixels = vec![0x33u8; (sw * sh * 4) as usize];
        let ffi = VaapiConfigFFI {
            width: sw, height: sh, cap_width: sw, cap_height: sh, fps: 60, gop: 10, cq: 24,
            target_bps: 1_000_000, max_bps: 2_000_000, buffer_bits: 500_000,
            is_hevc: 1, device: device.as_ptr(),
        };
        let engine = SafeVaapiEngine(unsafe { vaapi_create(&ffi) });
        assert!(!engine.0.is_null(), "VAAPI engine unavailable for POC wrap test");
        assert_eq!(unsafe { vaapi_upload_host_buffer(engine.0, small_pixels.as_ptr(), sw * 4) }, 0);
        assert_eq!(unsafe { vaapi_convert_input(engine.0, 0) }, 0);
        let idr = AtomicBool::new(false);
        let mut wrap_stream = Vec::new();
        let wrap_total = 4100usize;
        let mut idr_indices = Vec::new();
        for idx in 0..wrap_total {
            let pkt = vaapi_encode_nv12_packet(&engine, &idr, false).expect("wrap keepalive");
            if crate::video::is_independent_frame_for(&pkt, crate::capture_setting::VideoCodec::H265) {
                idr_indices.push(idx);
            }
            wrap_stream.extend_from_slice(&pkt);
        }
        assert_eq!(idr_indices, vec![0, 4095], "HEVC keepalive must emit IDR at frame 0 and POC boundary 4095 only");
        decode_and_count("hevc", wrap_stream, wrap_total);
    }
}
