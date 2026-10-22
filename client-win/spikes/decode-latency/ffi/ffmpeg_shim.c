/* Thin header-compiled ABI bridge, not a decoder. All decode work is libavcodec.
 * Never mirror AVCodecContext/AVFrame layouts in Rust. */
#define COBJMACROS
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <libavcodec/avcodec.h>
#include <libavutil/error.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_d3d11va.h>
#include <libavutil/version.h>

#if LIBAVCODEC_VERSION_MAJOR != 62 || LIBAVUTIL_VERSION_MAJOR != 60
#error "The accepted spike uses FFmpeg 8.1 (libavcodec 62 / libavutil 60) headers"
#endif

typedef struct BPFFmpegFrameInfo {
    void *texture;
    intptr_t array_slice;
    int64_t pts_100ns;
    int width;
    int height;
    int flags;
    int decode_error_flags;
} BPFFmpegFrameInfo;

static int bp_verify_immediate_context(ID3D11Device *device,
                                       ID3D11DeviceContext *actual)
{
    /* IID_IUnknown's value locally avoids an extra uuid.lib import. Canonical
     * identity is required rather than comparing arbitrary interface pointers. */
    static const GUID unknown_iid = {
        0x00000000, 0x0000, 0x0000,
        { 0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46 }
    };
    ID3D11DeviceContext *expected = NULL;
    IUnknown *expected_identity = NULL, *actual_identity = NULL;
    int result = AVERROR(EINVAL);
    HRESULT hr;
    if (!actual)
        return result;
    ID3D11Device_GetImmediateContext(device, &expected);
    if (!expected)
        goto done;
    hr = ID3D11DeviceContext_QueryInterface(expected, &unknown_iid,
                                           (void **)&expected_identity);
    if (FAILED(hr))
        goto done;
    hr = ID3D11DeviceContext_QueryInterface(actual, &unknown_iid,
                                           (void **)&actual_identity);
    if (FAILED(hr))
        goto done;
    if (expected_identity == actual_identity)
        result = 0;
done:
    if (actual_identity) IUnknown_Release(actual_identity);
    if (expected_identity) IUnknown_Release(expected_identity);
    if (expected) ID3D11DeviceContext_Release(expected);
    return result;
}

static enum AVPixelFormat bp_d3d11_format(AVCodecContext *ctx,
                                        const enum AVPixelFormat *formats)
{
    const enum AVPixelFormat *format;
    (void)ctx;
    for (format = formats; *format != AV_PIX_FMT_NONE; ++format) {
        if (*format == AV_PIX_FMT_D3D11)
            return AV_PIX_FMT_D3D11;
    }
    /* A failed hardware initialization may call this callback again with only
     * software formats. Returning NONE refuses that fallback. */
    return AV_PIX_FMT_NONE;
}

int bp_ffmpeg_create(int codec_kind, void *device, int width, int height,
                     AVCodecContext **out_context)
{
    const AVCodec *codec;
    AVCodecContext *ctx = NULL;
    AVBufferRef *device_ref = NULL;
    AVHWDeviceContext *hw_device;
    AVD3D11VADeviceContext *d3d11;
    const AVCodecHWConfig *config;
    int index, supported = 0, result = AVERROR(ENOMEM);

    if (!out_context || !device || width <= 0 || height <= 0)
        return AVERROR(EINVAL);
    *out_context = NULL;
    if (codec_kind != 0 && codec_kind != 1)
        return AVERROR(EINVAL);
    /* Detect a different-major DLL before accessing any library-owned struct. */
    if ((avcodec_version() >> 16) != LIBAVCODEC_VERSION_MAJOR ||
        (avutil_version() >> 16) != LIBAVUTIL_VERSION_MAJOR)
        return AVERROR(EINVAL);
    codec = avcodec_find_decoder_by_name(codec_kind == 0 ? "h264" : "hevc");
    if (!codec)
        return AVERROR_DECODER_NOT_FOUND;
    for (index = 0; (config = avcodec_get_hw_config(codec, index)); ++index) {
        if (config->device_type == AV_HWDEVICE_TYPE_D3D11VA &&
            config->pix_fmt == AV_PIX_FMT_D3D11 &&
            (config->methods & AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX)) {
            supported = 1;
            break;
        }
    }
    if (!supported)
        return AVERROR(ENOSYS);
    ctx = avcodec_alloc_context3(codec);
    if (!ctx)
        goto failed;
    device_ref = av_hwdevice_ctx_alloc(AV_HWDEVICE_TYPE_D3D11VA);
    if (!device_ref)
        goto failed;
    hw_device = (AVHWDeviceContext *)device_ref->data;
    d3d11 = (AVD3D11VADeviceContext *)hw_device->hwctx;
    /* av_hwdevice_ctx_alloc + init wraps the supplied presenter device; it does
     * not create another device. FFmpeg's destructor always Releases this ref,
     * including init failure, so transfer one additional reference to it. */
    ID3D11Device_AddRef((ID3D11Device *)device);
    d3d11->device = (ID3D11Device *)device;
    d3d11->BindFlags = D3D11_BIND_DECODER;
    result = av_hwdevice_ctx_init(device_ref);
    if (result < 0)
        goto failed;
    /* The presenter fences decode completion on its immediate context without
     * a texture copy. Verify FFmpeg issues decode work on that exact context. */
    result = bp_verify_immediate_context((ID3D11Device *)device,
                                         d3d11->device_context);
    if (result < 0) {
        av_log(ctx, AV_LOG_ERROR,
               "FFmpeg D3D11 context does not match the presenter immediate context\n");
        goto failed;
    }

    ctx->hw_device_ctx = device_ref;
    device_ref = NULL; /* Ownership transferred to AVCodecContext. */
    ctx->get_format = bp_d3d11_format;
    ctx->flags |= AV_CODEC_FLAG_LOW_DELAY;
    ctx->thread_count = 1;
    ctx->thread_type = 0; /* No frame threading or slice worker threading. */
    ctx->pkt_timebase.num = 1;
    ctx->pkt_timebase.den = 10000000;
    ctx->width = width;
    ctx->height = height;
    result = avcodec_open2(ctx, codec, NULL);
    if (result < 0)
        goto failed;
    if (!(ctx->flags & AV_CODEC_FLAG_LOW_DELAY) || ctx->thread_count != 1 ||
        ctx->active_thread_type != 0) {
        result = AVERROR(EINVAL);
        goto failed;
    }
    *out_context = ctx;
    return 0;

failed:
    av_buffer_unref(&device_ref);
    avcodec_free_context(&ctx);
    return result;
}

void bp_ffmpeg_free(AVCodecContext **ctx)
{
    avcodec_free_context(ctx);
}

int bp_ffmpeg_send(AVCodecContext *ctx, const uint8_t *data, int size, int64_t pts)
{
    AVPacket *packet;
    int result;
    if (!data || size <= 0 || pts == AV_NOPTS_VALUE)
        return AVERROR(EINVAL);
    packet = av_packet_alloc();
    if (!packet)
        return AVERROR(ENOMEM);
    /* av_new_packet allocates the required zero-filled bitstream padding and
     * reference-counted storage. No decoded surface transfer/copy occurs. */
    result = av_new_packet(packet, size);
    if (result >= 0) {
        memcpy(packet->data, data, (size_t)size);
        packet->pts = pts;
        packet->dts = AV_NOPTS_VALUE;
        packet->time_base = ctx->pkt_timebase;
        result = avcodec_send_packet(ctx, packet);
    }
    /* libavcodec retains its own packet ref after acceptance. On EAGAIN the
     * caller retains its AU and retries; this temporary packet is still freed. */
    av_packet_free(&packet);
    return result;
}

int bp_ffmpeg_send_eof(AVCodecContext *ctx)
{
    return avcodec_send_packet(ctx, NULL);
}

AVFrame *bp_ffmpeg_frame_alloc(void)
{
    return av_frame_alloc();
}

void bp_ffmpeg_frame_free(AVFrame **frame)
{
    av_frame_free(frame);
}

int bp_ffmpeg_receive(AVCodecContext *ctx, AVFrame *frame)
{
    return avcodec_receive_frame(ctx, frame);
}

int bp_ffmpeg_frame_info(const AVFrame *frame, BPFFmpegFrameInfo *out)
{
    const AVHWFramesContext *frames;
    if (!frame || !out || frame->format != AV_PIX_FMT_D3D11 ||
        !frame->hw_frames_ctx || !frame->data[0])
        return AVERROR(EINVAL);
    frames = (const AVHWFramesContext *)frame->hw_frames_ctx->data;
    if (frames->format != AV_PIX_FMT_D3D11 || frames->sw_format != AV_PIX_FMT_NV12)
        return AVERROR(EINVAL);
    if (frame->pts == AV_NOPTS_VALUE)
        return AVERROR(EINVAL);
    out->texture = frame->data[0];
    /* data[1] is an integer cast to a pointer, not memory to dereference. */
    out->array_slice = (intptr_t)frame->data[1];
    /* FFmpeg n8.1 ff_decode_frame_props_from_pkt copies packet->pts directly
     * into frame->pts, without rescaling. pkt_timebase is 1/10000000 here.
     * AVFrame.time_base remains optional in this API; it is not a substitute
     * timestamp unit. The coordinator also checks actual per-frame PTS IDs. */
    out->pts_100ns = frame->pts;
    out->width = frame->width;
    out->height = frame->height;
    out->flags = frame->flags & AV_FRAME_FLAG_CORRUPT;
    out->decode_error_flags = frame->decode_error_flags;
    return 0;
}

int bp_ffmpeg_error_kind(int result)
{
    if (result == AVERROR(EAGAIN)) return 1;
    if (result == AVERROR_EOF) return 2;
    return 0;
}

void bp_ffmpeg_error_text(int result, char *buffer, size_t capacity)
{
    if (av_strerror(result, buffer, capacity) < 0 && capacity)
        snprintf(buffer, capacity, "FFmpeg error %d", result);
}

void bp_ffmpeg_description(const AVCodecContext *ctx, char *buffer, size_t capacity)
{
    snprintf(buffer, capacity,
             "FFmpeg %s; libavcodec=%u; libavutil=%u; decoder=%s; d3d11va; "
             "presenter device attached; immediate_context_verified=true; "
             "low_delay=%d; thread_count=%d; "
             "thread_type=%d; active_thread_type=%d; pkt_timebase=%d/%d; "
             "AV_PIX_FMT_D3D11/NV12 required; software fallback refused",
             av_version_info(), avcodec_version(), avutil_version(), ctx->codec->name,
             !!(ctx->flags & AV_CODEC_FLAG_LOW_DELAY), ctx->thread_count,
             ctx->thread_type, ctx->active_thread_type,
             ctx->pkt_timebase.num, ctx->pkt_timebase.den);
}
