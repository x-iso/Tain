/*
 * A four-function C API over libavcodec's D3D11VA hardware decoder.
 *
 * # Why a C shim rather than Rust bindings
 *
 * Every Rust binding to ffmpeg runs bindgen, which needs libclang, which would
 * make LLVM a build prerequisite for everyone who clones this repository. It
 * would also mean declaring AVCodecContext and AVFrame on the Rust side, where
 * a field at the wrong offset is silent memory corruption rather than a
 * compile error.
 *
 * Compiling this file with the same MSVC toolchain Rust already requires solves
 * both. The C compiler reads ffmpeg's real headers, so every struct layout is
 * correct by construction, and Rust only ever sees the handful of plain
 * functions below.
 *
 * # Why the caller's D3D11 device
 *
 * The decoded frame is an ID3D11Texture2D, and it is about to be colour
 * converted by an ID3D11VideoProcessor on the guest's own device. If ffmpeg
 * created its own device, that texture would have to be shared across devices
 * before anything could read it. Handing our device in means the decode output
 * lands where it is already needed.
 */

#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_d3d11va.h>
#include <libavutil/opt.h>
#include <string.h>
#include <stdlib.h>

struct DbDecoder {
    AVCodecContext *ctx;
    AVBufferRef *hw_device;
    AVPacket *packet;
    /*
     * Two frames, alternating.
     *
     * The caller submits a GPU blit that reads the decoded surface and does not
     * wait for it — that is what makes the readback cheap. Releasing the frame
     * straight away would let libavcodec hand the same surface back to the
     * decoder and overwrite the picture while the blit is still queued.
     *
     * Holding the previous one as well gives every blit a full frame period to
     * execute before its source can be reused. ffmpeg's surface pool is deep
     * enough that one extra costs nothing.
     */
    AVFrame *frames[2];
    int slot;
    int frame_held;
    /* Padded copy of the packet being decoded; see db_decoder_send. */
    unsigned char *buf;
    int buf_cap;
};

typedef struct DbDecoder DbDecoder;

/* Codec selector, kept as a plain int so the Rust side declares no enums. */
#define DB_CODEC_H264 0
#define DB_CODEC_HEVC 1
#define DB_CODEC_AV1  2

/* The construction path unwinds through the destructor, which is defined at the
 * bottom; without this C infers a return type of int and then rejects it. */
void db_decoder_free(DbDecoder *d);

static void db_fail(char *err, int err_len, const char *msg)
{
    if (err && err_len > 0) {
        strncpy(err, msg, (size_t)err_len - 1);
        err[err_len - 1] = '\0';
    }
}

/*
 * Pin the hardware pixel format.
 *
 * Without this callback libavcodec falls back to a software pixel format the
 * moment anything about the hardware path is not to its liking — which is the
 * same silent downgrade that made Media Foundation look like it was working.
 * Returning AV_PIX_FMT_NONE instead makes that failure loud.
 */
static enum AVPixelFormat db_get_format(AVCodecContext *ctx,
                                        const enum AVPixelFormat *formats)
{
    av_log(ctx, AV_LOG_VERBOSE,
           "db: get_format codec_id=%d profile=%d level=%d sw_pix_fmt=%d %dx%d\n",
           (int)ctx->codec_id, ctx->profile, ctx->level, (int)ctx->sw_pix_fmt,
           ctx->width, ctx->height);
    for (const enum AVPixelFormat *p = formats; *p != AV_PIX_FMT_NONE; p++) {
        if (*p == AV_PIX_FMT_D3D11) {
            return AV_PIX_FMT_D3D11;
        }
    }
    return AV_PIX_FMT_NONE;
}

DbDecoder *db_decoder_new(void *d3d11_device, int codec, char *err, int err_len)
{
    enum AVCodecID id;
    switch (codec) {
    case DB_CODEC_HEVC: id = AV_CODEC_ID_HEVC; break;
    case DB_CODEC_AV1:  id = AV_CODEC_ID_AV1;  break;
    default:            id = AV_CODEC_ID_H264; break;
    }

    if (getenv("DB_FFMPEG_DEBUG")) {
        av_log_set_level(AV_LOG_DEBUG);
    }

    const AVCodec *decoder = avcodec_find_decoder(id);
    if (!decoder) {
        db_fail(err, err_len, "libavcodec has no decoder for this codec");
        return NULL;
    }

    DbDecoder *d = av_mallocz(sizeof(DbDecoder));
    if (!d) {
        db_fail(err, err_len, "out of memory");
        return NULL;
    }

    d->hw_device = av_hwdevice_ctx_alloc(AV_HWDEVICE_TYPE_D3D11VA);
    if (!d->hw_device) {
        db_fail(err, err_len, "could not allocate a D3D11VA device context");
        db_decoder_free(d);
        return NULL;
    }

    /*
     * Adopt the caller's device. ffmpeg takes ownership of what it finds here
     * and releases it with the context, so the reference count is raised to
     * cover that: the caller keeps its own reference either way.
     *
     * There is deliberately no "let ffmpeg make its own device" fallback. The
     * decoded texture has to be readable by the caller's video processor, and a
     * decoder running on a device that processor cannot touch would hand back
     * surfaces that blit to garbage rather than failing.
     */
    AVHWDeviceContext *hw = (AVHWDeviceContext *)d->hw_device->data;
    AVD3D11VADeviceContext *d3d = hw->hwctx;
    d3d->device = (ID3D11Device *)d3d11_device;
    d3d->device->lpVtbl->AddRef(d3d->device);

    int rc = av_hwdevice_ctx_init(d->hw_device);
    if (rc < 0) {
        db_fail(err, err_len, "could not initialise the D3D11VA device");
        db_decoder_free(d);
        return NULL;
    }

    d->ctx = avcodec_alloc_context3(decoder);
    if (!d->ctx) {
        db_fail(err, err_len, "could not allocate a decoder context");
        db_decoder_free(d);
        return NULL;
    }

    d->ctx->hw_device_ctx = av_buffer_ref(d->hw_device);
    d->ctx->get_format = db_get_format;
    /* Emit each picture as soon as it is decoded rather than reordering. */
    d->ctx->flags |= AV_CODEC_FLAG_LOW_DELAY;
    d->ctx->flags2 |= AV_CODEC_FLAG2_FAST;
    d->ctx->thread_count = 1;
    /*
     * # Without this there is no hardware decode at all
     *
     * ffmpeg only offers DXVA for H.264 profiles on a fixed list: Constrained
     * Baseline, Main and High. This project's encoder emits plain **Baseline**
     * (profile_idc 66) on purpose, because OpenH264 is the software fallback on
     * the other side and that is what it takes -- see codec_mf's header.
     *
     * Baseline is not on ffmpeg's list, so the driver was refused and every
     * frame fell back to software with nothing but "No decoder device for codec
     * found" at verbose log level to say why.
     *
     * Overriding is safe here rather than merely convenient: Baseline is a
     * subset of Main apart from arbitrary slice ordering and redundant slices,
     * neither of which any hardware encoder emits, and the list is a
     * conservative default rather than a hardware limit.
     */
    if (id == AV_CODEC_ID_H264) {
        d->ctx->hwaccel_flags |= AV_HWACCEL_FLAG_ALLOW_PROFILE_MISMATCH;
    }

    rc = avcodec_open2(d->ctx, decoder, NULL);
    if (rc < 0) {
        db_fail(err, err_len, "avcodec_open2 refused the hardware decoder");
        db_decoder_free(d);
        return NULL;
    }

    d->packet = av_packet_alloc();
    d->frames[0] = av_frame_alloc();
    d->frames[1] = av_frame_alloc();
    if (!d->packet || !d->frames[0] || !d->frames[1]) {
        db_fail(err, err_len, "out of memory");
        db_decoder_free(d);
        return NULL;
    }
    return d;
}

const char *db_decoder_name(DbDecoder *d)
{
    if (!d || !d->ctx || !d->ctx->codec) {
        return "none";
    }
    return d->ctx->codec->name;
}

int db_decoder_send(DbDecoder *d, const unsigned char *data, int len)
{
    if (!d || !d->ctx) {
        return -1;
    }
    /*
     * # Why this copies rather than pointing at the caller's buffer
     *
     * libavcodec's bitstream readers deliberately read past the end of the
     * data, by up to AV_INPUT_BUFFER_PADDING_SIZE bytes, and require those
     * bytes to exist and be zero. Handing them an exactly-sized buffer does
     * not fault — it returns AVERROR_INVALIDDATA on the first packet, which
     * looks like a corrupt stream rather than a violated precondition.
     *
     * The buffer is reused across frames, so this is one memcpy of a packet
     * that is tens of kilobytes: microseconds against a decode of milliseconds.
     */
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
 * Returns 1 when a picture is ready, 0 when more input is needed, and a
 * negative ffmpeg error code otherwise.
 *
 * On 1, *texture is an ID3D11Texture2D the decoder still owns and *slice is its
 * index within that texture's array. Both are valid until db_decoder_release.
 */
int db_decoder_receive(DbDecoder *d, void **texture, int *slice, int *width,
                       int *height)
{
    if (!d || !d->ctx) {
        return -1;
    }
    /* Take the other slot, releasing whatever was in it from two calls ago —
     * its blit finished long since. */
    int next = d->slot ^ 1;
    av_frame_unref(d->frames[next]);

    int rc = avcodec_receive_frame(d->ctx, d->frames[next]);
    if (rc == AVERROR(EAGAIN) || rc == AVERROR_EOF) {
        return 0;
    }
    if (rc < 0) {
        return rc;
    }
    d->slot = next;
    d->frame_held = 1;

    AVFrame *frame = d->frames[d->slot];
    if (frame->format != AV_PIX_FMT_D3D11) {
        /* The hardware path fell through to software; say so rather than
         * handing back a pointer that is not a texture. */
        return -1;
    }

    *texture = (void *)frame->data[0];
    *slice = (int)(intptr_t)frame->data[1];
    *width = frame->width;
    *height = frame->height;
    return 1;
}

/*
 * Release everything held.
 *
 * Only for shutdown. Per-frame release is automatic and one frame behind — see
 * the note on `frames` — because releasing eagerly would recycle a surface a
 * queued blit is still reading from.
 */
void db_decoder_release(DbDecoder *d)
{
    if (!d) {
        return;
    }
    for (int i = 0; i < 2; i++) {
        if (d->frames[i]) {
            av_frame_unref(d->frames[i]);
        }
    }
    d->frame_held = 0;
}

void db_decoder_free(DbDecoder *d)
{
    if (!d) {
        return;
    }
    for (int i = 0; i < 2; i++) {
        if (d->frames[i]) {
            av_frame_free(&d->frames[i]);
        }
    }
    if (d->packet) {
        av_packet_free(&d->packet);
    }
    if (d->ctx) {
        avcodec_free_context(&d->ctx);
    }
    if (d->hw_device) {
        av_buffer_unref(&d->hw_device);
    }
    if (d->buf) {
        av_free(d->buf);
    }
    av_free(d);
}
