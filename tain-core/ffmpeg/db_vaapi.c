/*
 * VA-API decode and encode over libavcodec, for Linux.
 *
 * The same shim pattern as db_decode.c and for the same reasons: the C
 * compiler reads ffmpeg's real headers, so no struct layout is declared on the
 * Rust side, and no bindgen means no libclang in the build prerequisites.
 *
 * # Why the picture comes back as NV12
 *
 * The Windows decoder hands Rust a D3D11 texture and converts it on the GPU.
 * Here the decoded surface is copied back as NV12 and converted to RGBA by the
 * caller, which is the readback the Windows path also falls back to. Drawing
 * the surface without the copy means exporting it as a DMA-BUF and importing
 * it into the GL context as an EGLImage; that is the next step, and it changes
 * nothing in this file's decode half but what receive returns.
 *
 * # libva is not linked
 *
 * This file is linked into libtain-media.so with libva's symbols left
 * undefined, and that library is opened with lazy binding. The runtime opens
 * libva.so.2 and libva-drm.so.2 globally first when they exist, and only then
 * calls anything here that reaches VA-API. A machine without libva therefore
 * still loads the library and still gets Opus; see media_runtime_linux.rs.
 */

#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_vaapi.h>
#include <libavutil/opt.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* Codec selector, the same values as db_decode.c. */
#define DB_CODEC_H264 0
#define DB_CODEC_HEVC 1
#define DB_CODEC_AV1  2

/* Bumped when a signature below changes, so a library substituted beside the
 * executable that no longer matches is refused rather than called. */
int db_media_abi(void)
{
    return 1;
}

static void db_fail(char *err, int err_len, const char *msg)
{
    if (err && err_len > 0) {
        strncpy(err, msg, (size_t)err_len - 1);
        err[err_len - 1] = '\0';
    }
}

static enum AVCodecID db_codec_id(int codec)
{
    switch (codec) {
    case DB_CODEC_HEVC: return AV_CODEC_ID_HEVC;
    case DB_CODEC_AV1:  return AV_CODEC_ID_AV1;
    default:            return AV_CODEC_ID_H264;
    }
}

/*
 * A VA-API device on the render node ffmpeg finds first, or the one named in
 * TAIN_VAAPI_DEVICE.
 *
 * The override is for machines with two GPUs, where the first render node is
 * not always the one with a working driver — a laptop's discrete NVIDIA GPU
 * has no VA-API driver of its own, and ordering decides which one this gets.
 */
static int db_open_device(AVBufferRef **device, char *err, int err_len)
{
    const char *node = getenv("TAIN_VAAPI_DEVICE");
    if (getenv("DB_FFMPEG_DEBUG")) {
        av_log_set_level(AV_LOG_DEBUG);
    }
    int rc = av_hwdevice_ctx_create(device, AV_HWDEVICE_TYPE_VAAPI,
                                    node && *node ? node : NULL, NULL, 0);
    if (rc < 0) {
        char why[128];
        av_strerror(rc, why, sizeof why);
        char msg[256];
        snprintf(msg, sizeof msg,
                 "no VA-API device (%s) — is there a VA-API driver for this GPU?", why);
        db_fail(err, err_len, msg);
        return rc;
    }
    return 0;
}

/* What the driver calls itself, for a log line and the stats overlay. */
static void db_describe(AVBufferRef *device, char *out, size_t out_len)
{
    AVHWDeviceContext *hw = (AVHWDeviceContext *)device->data;
    AVVAAPIDeviceContext *va = hw->hwctx;
    const char *vendor = vaQueryVendorString(va->display);
    snprintf(out, out_len, "VA-API, %s", vendor ? vendor : "unknown driver");
}

/* ── Decode ─────────────────────────────────────────────────────────────── */

struct DbVaDecoder {
    AVCodecContext *ctx;
    AVBufferRef *device;
    AVPacket *packet;
    AVFrame *hw;
    AVFrame *sw;
    unsigned char *buf;
    int buf_cap;
    char name[256];
};

typedef struct DbVaDecoder DbVaDecoder;

void db_va_decoder_free(DbVaDecoder *d);

/* Insist on VA-API surfaces, for the reason db_decode.c insists on D3D11:
 * libavcodec otherwise falls back to software without saying so. */
static enum AVPixelFormat db_va_get_format(AVCodecContext *ctx,
                                           const enum AVPixelFormat *formats)
{
    (void)ctx;
    for (const enum AVPixelFormat *p = formats; *p != AV_PIX_FMT_NONE; p++) {
        if (*p == AV_PIX_FMT_VAAPI) {
            return AV_PIX_FMT_VAAPI;
        }
    }
    return AV_PIX_FMT_NONE;
}

DbVaDecoder *db_va_decoder_new(int codec, char *err, int err_len)
{
    enum AVCodecID id = db_codec_id(codec);

    /* The native "av1" decoder carries the hwaccels; libdav1d, preferred when
     * it is compiled in, has none. See db_decode.c. */
    const AVCodec *decoder = NULL;
    if (id == AV_CODEC_ID_AV1) {
        decoder = avcodec_find_decoder_by_name("av1");
    }
    if (!decoder) {
        decoder = avcodec_find_decoder(id);
    }
    if (!decoder) {
        db_fail(err, err_len, "libavcodec has no decoder for this codec");
        return NULL;
    }

    DbVaDecoder *d = av_mallocz(sizeof(DbVaDecoder));
    if (!d) {
        db_fail(err, err_len, "out of memory");
        return NULL;
    }
    if (db_open_device(&d->device, err, err_len) < 0) {
        db_va_decoder_free(d);
        return NULL;
    }

    d->ctx = avcodec_alloc_context3(decoder);
    if (!d->ctx) {
        db_fail(err, err_len, "could not allocate a decoder context");
        db_va_decoder_free(d);
        return NULL;
    }
    d->ctx->hw_device_ctx = av_buffer_ref(d->device);
    d->ctx->get_format = db_va_get_format;
    d->ctx->flags |= AV_CODEC_FLAG_LOW_DELAY;
    d->ctx->flags2 |= AV_CODEC_FLAG2_FAST;
    d->ctx->thread_count = 1;
    /* The host's H.264 is Baseline on purpose (OpenH264 is the other side's
     * fallback), and ffmpeg's hwaccel profile list leaves Baseline out. The
     * same override as db_decode.c, for the same reason. */
    if (id == AV_CODEC_ID_H264) {
        d->ctx->hwaccel_flags |= AV_HWACCEL_FLAG_ALLOW_PROFILE_MISMATCH;
    }

    if (avcodec_open2(d->ctx, decoder, NULL) < 0) {
        db_fail(err, err_len, "avcodec_open2 refused the VA-API decoder");
        db_va_decoder_free(d);
        return NULL;
    }

    d->packet = av_packet_alloc();
    d->hw = av_frame_alloc();
    d->sw = av_frame_alloc();
    if (!d->packet || !d->hw || !d->sw) {
        db_fail(err, err_len, "out of memory");
        db_va_decoder_free(d);
        return NULL;
    }
    char driver[192];
    db_describe(d->device, driver, sizeof driver);
    snprintf(d->name, sizeof d->name, "%s (%s)", decoder->name, driver);
    return d;
}

const char *db_va_decoder_name(DbVaDecoder *d)
{
    return d ? d->name : "none";
}

/* The packet is copied into a padded buffer first; see db_decoder_send for why
 * libavcodec needs the padding. */
int db_va_decoder_send(DbVaDecoder *d, const unsigned char *data, int len)
{
    if (!d || !d->ctx) {
        return -1;
    }
    int needed = len + AV_INPUT_BUFFER_PADDING_SIZE;
    if (d->buf_cap < needed) {
        unsigned char *grown = av_realloc(d->buf, (size_t)needed);
        if (!grown) {
            return AVERROR(ENOMEM);
        }
        d->buf = grown;
        d->buf_cap = needed;
    }
    memcpy(d->buf, data, (size_t)len);
    memset(d->buf + len, 0, AV_INPUT_BUFFER_PADDING_SIZE);
    d->packet->data = d->buf;
    d->packet->size = len;
    int rc = avcodec_send_packet(d->ctx, d->packet);
    d->packet->data = NULL;
    d->packet->size = 0;
    return rc;
}

/*
 * Returns 1 with a picture, 0 when more input is needed, and a negative ffmpeg
 * error otherwise.
 *
 * On 1 the planes are NV12 in memory this decoder owns, valid until the next
 * call to receive or free. 8-bit only: the hosts this talks to encode 8-bit.
 */
int db_va_decoder_receive(DbVaDecoder *d, const unsigned char **y, int *y_stride,
                          const unsigned char **uv, int *uv_stride, int *width,
                          int *height)
{
    if (!d || !d->ctx) {
        return -1;
    }
    av_frame_unref(d->hw);
    int rc = avcodec_receive_frame(d->ctx, d->hw);
    if (rc == AVERROR(EAGAIN) || rc == AVERROR_EOF) {
        return 0;
    }
    if (rc < 0) {
        return rc;
    }
    if (d->hw->format != AV_PIX_FMT_VAAPI) {
        return -1;
    }
    av_frame_unref(d->sw);
    d->sw->format = AV_PIX_FMT_NV12;
    rc = av_hwframe_transfer_data(d->sw, d->hw, 0);
    if (rc < 0) {
        return rc;
    }
    if (d->sw->format != AV_PIX_FMT_NV12) {
        return -1;
    }
    *y = d->sw->data[0];
    *y_stride = d->sw->linesize[0];
    *uv = d->sw->data[1];
    *uv_stride = d->sw->linesize[1];
    *width = d->sw->width;
    *height = d->sw->height;
    return 1;
}

void db_va_decoder_free(DbVaDecoder *d)
{
    if (!d) {
        return;
    }
    av_frame_free(&d->hw);
    av_frame_free(&d->sw);
    av_packet_free(&d->packet);
    avcodec_free_context(&d->ctx);
    av_buffer_unref(&d->device);
    av_free(d->buf);
    av_free(d);
}

/* ── Encode ─────────────────────────────────────────────────────────────── */

struct DbVaEncoder {
    AVCodecContext *ctx;
    AVBufferRef *device;
    AVBufferRef *frames;
    AVFrame *sw;
    AVFrame *hw;
    AVPacket *packet;
    unsigned char *out;
    int out_cap;
    int64_t pts;
    char name[256];
};

typedef struct DbVaEncoder DbVaEncoder;

void db_va_encoder_free(DbVaEncoder *e);

static const char *db_encoder_name(int codec)
{
    switch (codec) {
    case DB_CODEC_HEVC: return "hevc_vaapi";
    case DB_CODEC_AV1:  return "av1_vaapi";
    default:            return "h264_vaapi";
    }
}

/*
 * One attempt at opening the encoder, in normal or low-power mode.
 *
 * Some GPUs encode a codec only in one of the two — recent Intel parts have
 * only the low-power (VDENC) entrypoint for HEVC and AV1, older ones only the
 * normal one for H.264 — and ffmpeg tries only the one it is told. So the
 * caller tries both.
 */
static int db_va_encoder_open(DbVaEncoder *e, const AVCodec *codec, int which,
                              int width, int height, int fps, int bitrate_kbps,
                              int gop, int low_power, const char *profile)
{
    avcodec_free_context(&e->ctx);
    e->ctx = avcodec_alloc_context3(codec);
    if (!e->ctx) {
        return AVERROR(ENOMEM);
    }
    AVCodecContext *c = e->ctx;
    c->width = width;
    c->height = height;
    c->pix_fmt = AV_PIX_FMT_VAAPI;
    c->sw_pix_fmt = AV_PIX_FMT_NV12;
    c->hw_frames_ctx = av_buffer_ref(e->frames);
    c->time_base = (AVRational){1, fps > 0 ? fps : 60};
    c->framerate = (AVRational){fps > 0 ? fps : 60, 1};
    /* No B-frames: every frame goes out as soon as it is encoded, which is
     * the whole contract of VideoEncoder::encode_frame. */
    c->max_b_frames = 0;
    c->gop_size = gop > 0 ? gop : 0x7fffffff;
    int64_t bps = (int64_t)(bitrate_kbps > 0 ? bitrate_kbps : 8000) * 1000;
    c->bit_rate = bps;
    c->rc_max_rate = bps + bps / 2;
    /* Half a second of buffer: enough room for a keyframe, small enough that a
     * burst cannot queue far behind the pointer. */
    c->rc_buffer_size = (int)(bps / 2);
    /* Range and matrix as the CPU converter writes them; see bgra_to_nv12. */
    c->color_range = AVCOL_RANGE_MPEG;
    c->colorspace = AVCOL_SPC_BT470BG;
    c->color_primaries = AVCOL_PRI_BT470BG;
    c->color_trc = AVCOL_TRC_BT709;

    av_opt_set_int(c->priv_data, "async_depth", 1, 0);
    av_opt_set(c->priv_data, "rc_mode", "VBR", 0);
    av_opt_set_int(c->priv_data, "low_power", low_power, 0);
    (void)which;
    if (profile) {
        av_opt_set(c->priv_data, "profile", profile, 0);
    }
    return avcodec_open2(c, codec, NULL);
}

DbVaEncoder *db_va_encoder_new(int codec, int width, int height, int fps,
                               int bitrate_kbps, int gop, char *err, int err_len)
{
    const AVCodec *encoder = avcodec_find_encoder_by_name(db_encoder_name(codec));
    if (!encoder) {
        db_fail(err, err_len, "this ffmpeg has no VA-API encoder for the codec");
        return NULL;
    }
    if (width <= 0 || height <= 0 || (width & 1) || (height & 1)) {
        db_fail(err, err_len, "the frame size must be even and non-zero");
        return NULL;
    }

    DbVaEncoder *e = av_mallocz(sizeof(DbVaEncoder));
    if (!e) {
        db_fail(err, err_len, "out of memory");
        return NULL;
    }
    if (db_open_device(&e->device, err, err_len) < 0) {
        db_va_encoder_free(e);
        return NULL;
    }

    e->frames = av_hwframe_ctx_alloc(e->device);
    if (!e->frames) {
        db_fail(err, err_len, "could not allocate a VA-API frame pool");
        db_va_encoder_free(e);
        return NULL;
    }
    AVHWFramesContext *pool = (AVHWFramesContext *)e->frames->data;
    pool->format = AV_PIX_FMT_VAAPI;
    pool->sw_format = AV_PIX_FMT_NV12;
    pool->width = width;
    pool->height = height;
    pool->initial_pool_size = 8;
    if (av_hwframe_ctx_init(e->frames) < 0) {
        db_fail(err, err_len, "the driver refused an NV12 surface pool of this size");
        db_va_encoder_free(e);
        return NULL;
    }

    /*
     * H.264 is asked for in Constrained Baseline first, as the Windows encoder
     * emits it, so a guest that falls back to OpenH264 can still decode it.
     * Main is the fallback for a GPU that will not encode Baseline: every
     * hardware decoder takes it, and OpenH264 may not. The other codecs take
     * the encoder's default profile.
     */
    const char *h264_profiles[] = {"constrained_baseline", "main"};
    const char *default_profile[] = {NULL};
    const char **profiles = codec == DB_CODEC_H264 ? h264_profiles : default_profile;
    int profile_count = codec == DB_CODEC_H264 ? 2 : 1;
    int rc = AVERROR(ENOSYS);
    for (int p = 0; p < profile_count && rc < 0; p++) {
        for (int low_power = 0; low_power <= 1 && rc < 0; low_power++) {
            rc = db_va_encoder_open(e, encoder, codec, width, height, fps, bitrate_kbps,
                                    gop, low_power, profiles[p]);
        }
    }
    if (rc < 0) {
        char why[128];
        av_strerror(rc, why, sizeof why);
        char msg[256];
        snprintf(msg, sizeof msg, "the GPU will not encode this codec (%s)", why);
        db_fail(err, err_len, msg);
        db_va_encoder_free(e);
        return NULL;
    }

    e->sw = av_frame_alloc();
    e->hw = av_frame_alloc();
    e->packet = av_packet_alloc();
    if (!e->sw || !e->hw || !e->packet) {
        db_fail(err, err_len, "out of memory");
        db_va_encoder_free(e);
        return NULL;
    }
    char driver[192];
    db_describe(e->device, driver, sizeof driver);
    snprintf(e->name, sizeof e->name, "%s (%s)", encoder->name, driver);
    return e;
}

const char *db_va_encoder_name(DbVaEncoder *e)
{
    return e ? e->name : "none";
}

/*
 * Encode one NV12 frame, tightly packed at the size the encoder was made for.
 *
 * Returns 0 with *out and *out_len set (an empty packet is possible and is not an
 * error), or a negative ffmpeg error. *out stays valid until the next call.
 */
int db_va_encoder_encode(DbVaEncoder *e, const unsigned char *nv12, int force_keyframe,
                         const unsigned char **out, int *out_len, int *is_keyframe)
{
    if (!e || !e->ctx) {
        return -1;
    }
    *out = NULL;
    *out_len = 0;
    *is_keyframe = 0;

    int width = e->ctx->width;
    int height = e->ctx->height;
    av_frame_unref(e->sw);
    e->sw->format = AV_PIX_FMT_NV12;
    e->sw->width = width;
    e->sw->height = height;
    e->sw->data[0] = (uint8_t *)nv12;
    e->sw->linesize[0] = width;
    e->sw->data[1] = (uint8_t *)nv12 + (size_t)width * (size_t)height;
    e->sw->linesize[1] = width;

    av_frame_unref(e->hw);
    int rc = av_hwframe_get_buffer(e->frames, e->hw, 0);
    if (rc < 0) {
        return rc;
    }
    rc = av_hwframe_transfer_data(e->hw, e->sw, 0);
    /* The software frame only borrowed the caller's buffer. */
    e->sw->data[0] = NULL;
    e->sw->data[1] = NULL;
    if (rc < 0) {
        return rc;
    }
    e->hw->pts = e->pts++;
    if (force_keyframe) {
        e->hw->pict_type = AV_PICTURE_TYPE_I;
    }
    rc = avcodec_send_frame(e->ctx, e->hw);
    av_frame_unref(e->hw);
    if (rc < 0) {
        return rc;
    }

    int total = 0;
    for (;;) {
        rc = avcodec_receive_packet(e->ctx, e->packet);
        if (rc == AVERROR(EAGAIN) || rc == AVERROR_EOF) {
            break;
        }
        if (rc < 0) {
            return rc;
        }
        int needed = total + e->packet->size;
        if (e->out_cap < needed) {
            unsigned char *grown = av_realloc(e->out, (size_t)needed);
            if (!grown) {
                av_packet_unref(e->packet);
                return AVERROR(ENOMEM);
            }
            e->out = grown;
            e->out_cap = needed;
        }
        memcpy(e->out + total, e->packet->data, (size_t)e->packet->size);
        total = needed;
        if (e->packet->flags & AV_PKT_FLAG_KEY) {
            *is_keyframe = 1;
        }
        av_packet_unref(e->packet);
    }
    *out = e->out;
    *out_len = total;
    return 0;
}

void db_va_encoder_free(DbVaEncoder *e)
{
    if (!e) {
        return;
    }
    av_frame_free(&e->sw);
    av_frame_free(&e->hw);
    av_packet_free(&e->packet);
    avcodec_free_context(&e->ctx);
    av_buffer_unref(&e->frames);
    av_buffer_unref(&e->device);
    av_free(e->out);
    av_free(e);
}

/* ── What this GPU can do ──────────────────────────────────────────────── */

static int db_has_profile(VADisplay display, VAProfile profile, int encode)
{
    int max = vaMaxNumEntrypoints(display);
    if (max <= 0) {
        return 0;
    }
    VAEntrypoint *entrypoints = av_malloc_array((size_t)max, sizeof(VAEntrypoint));
    if (!entrypoints) {
        return 0;
    }
    int count = 0;
    int found = 0;
    if (vaQueryConfigEntrypoints(display, profile, entrypoints, &count) == VA_STATUS_SUCCESS) {
        for (int i = 0; i < count; i++) {
            if (!encode && entrypoints[i] == VAEntrypointVLD) {
                found = 1;
            }
            if (encode && (entrypoints[i] == VAEntrypointEncSlice ||
                           entrypoints[i] == VAEntrypointEncSliceLP)) {
                found = 1;
            }
        }
    }
    av_free(entrypoints);
    return found;
}

/*
 * Whether this machine's GPU can decode (encode = 0) or encode (encode = 1)
 * the codec through VA-API: 1 yes, 0 no, negative when there is no VA-API
 * device at all. *driver, when given, receives the driver's own description.
 *
 * Asked of the driver's profile and entrypoint lists, so it is cheap and does
 * not build a codec. Whether ffmpeg in this build has the matching encoder is
 * checked as well, since a GPU that can encode AV1 is no use to an ffmpeg
 * without av1_vaapi.
 */
int db_va_supports(int codec, int encode, char *driver, int driver_len)
{
    if (encode && !avcodec_find_encoder_by_name(db_encoder_name(codec))) {
        return 0;
    }
    AVBufferRef *device = NULL;
    if (db_open_device(&device, NULL, 0) < 0) {
        return -1;
    }
    if (driver && driver_len > 0) {
        db_describe(device, driver, (size_t)driver_len);
    }
    AVHWDeviceContext *hw = (AVHWDeviceContext *)device->data;
    VADisplay display = ((AVVAAPIDeviceContext *)hw->hwctx)->display;

    int found = 0;
    switch (codec) {
    case DB_CODEC_HEVC:
        found = db_has_profile(display, VAProfileHEVCMain, encode);
        break;
    case DB_CODEC_AV1:
        found = db_has_profile(display, VAProfileAV1Profile0, encode);
        break;
    default:
        found = db_has_profile(display, VAProfileH264ConstrainedBaseline, encode) ||
                db_has_profile(display, VAProfileH264Main, encode) ||
                db_has_profile(display, VAProfileH264High, encode);
        break;
    }
    av_buffer_unref(&device);
    return found;
}
