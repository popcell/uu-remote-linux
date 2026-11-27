#ifndef UU_VAAPI_ENGINE_H
#define UU_VAAPI_ENGINE_H

#include <stdint.h>
#include <stddef.h>
#include "dmabuf_frame.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct VaapiEngine VaapiEngine;

typedef struct {
    uint32_t width;      // encode (output) size
    uint32_t height;
    uint32_t cap_width;  // capture (input) size; VPP scales cap -> width
    uint32_t cap_height;
    uint32_t fps;
    uint32_t gop;
    uint32_t cq;         // minimum QP (viewer quality preset; 0 defaults to 16)
    int64_t target_bps;
    int64_t max_bps;     // VBR ceiling (viewer preset maxrate)
    int64_t buffer_bits; // HRD buffer size
    int is_hevc;
    const char *device;  // DRM render node (UU_VAAPI_DEVICE); NULL = /dev/dri/renderD128
} VaapiConfig;

VaapiEngine* vaapi_create(const VaapiConfig* config);

// Import a DRI3 / GBM DMA-BUF fd as the zero-copy input surface
int vaapi_import_dma_buffer(VaapiEngine* engine, int dma_fd, uint32_t stride);
int vaapi_import_frame(VaapiEngine* engine, const UuDmaFrame *frame);

// Host fallback: upload ARGB host memory if DMA-BUF is unavailable
int vaapi_upload_host_buffer(VaapiEngine* engine, const uint8_t* host_ptr, uint32_t pitch);

// Non-zero when the selected VAAPI rate-control mode honors bitrate caps (VBR/CBR, not CQP).
int vaapi_supports_rate_control(const VaapiEngine* engine);

// Non-zero when external PipeWire DMA-BUF VPP can be synchronized with a bounded vaSyncSurface2 wait.
int vaapi_supports_bounded_sync(const VaapiEngine* engine);

/* Convert the current imported/uploaded RGB input into the engine's private NV12 surface and wait
 * (bounded to 500 ms) for VPP completion. When release_input is non-zero (ephemeral PipeWire
 * DMA-BUF frames), requires verified vaSyncSurface2 VPP completion support (returns -14 before
 * submission if absent), destroys the external RGB surface wrapper only once VPP is proven
 * complete, and returns:
 *   0: completed, input cleared, safe to release DMA-BUF lease and encode;
 * -10: VPP failed before submission (input cleared);
 * -12: VPP exceeded 500 ms but returned VA_STATUS_SUCCESS during drain (input cleared, safe to
 *      release lease and tear down the VAAPI pipeline);
 * -13: VPP submission may have reached the GPU, but completion could not be confirmed (submission
 *      error, timeout past drain, or sync error). Abandon the PipeWire DMA-BUF lease before tearing
 *      down the VAAPI engine;
 * -14: release_input requested but driver lacks verified bounded VPP sync (nothing submitted). */
int vaapi_convert_input(VaapiEngine* engine, int release_input);

/* Encode the engine's prepared NV12 surface without touching any external RGB input.
 * When count_gop is zero (unchanged static keepalive frame), periodic GOP IDRs are suppressed and
 * since_idr does not advance, while force_idr / idr_pending / frame 0 still emit an IDR. */
int vaapi_encode_nv12(
    VaapiEngine* engine,
    int force_idr,
    int count_gop,
    const uint8_t** out_data,
    uint32_t* out_len
);

// Encode one frame (convenience wrapper: VPP convert keeping input + encode counting toward GOP)
int vaapi_encode_frame(
    VaapiEngine* engine,
    int force_idr,
    const uint8_t** out_data,
    uint32_t* out_len
);

/* Retune rate control: VBR ceiling, target, HRD buffer size and the QP floor (the viewer's quality
 * preset, what FFmpeg calls -qmin on this encoder). The context, surfaces and imported buffer stay
 * and the change lands on the next IDR, so a send-side bitrate step costs one keyframe instead of the
 * pipeline teardown it replaces. */
void vaapi_set_rate(VaapiEngine* engine, int64_t target_bps, int64_t max_bps, int64_t buffer_bits, uint32_t cq);

void vaapi_destroy(VaapiEngine* engine);

#ifdef __cplusplus
}
#endif

#endif // UU_VAAPI_ENGINE_H
