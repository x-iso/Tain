/*
 * Opus encode and decode, over libavcodec.
 *
 * Same shim pattern and the same reasons as db_decode.c: the C compiler reads
 * ffmpeg's real headers so no struct layout is ever declared on the Rust side,
 * and no bindgen means no libclang in the build prerequisites.
 *
 * # Why Opus
 *
 * It is what WebRTC, Discord and every low-latency game streamer uses for this
 * exact job, and libopus is already inside the avcodec this project links for
 * video decode, so it costs nothing new.
 *
 * The settings below are chosen for game and desktop audio on a link that will
 * sometimes drop packets:
 *
 * - **application=audio**, not voip. VOIP mode optimises for speech and audibly
 *   mangles music.
 * - **10 ms frames.** 2.5 ms would shave latency but costs a lot of efficiency
 *   at these bitrates; 20 ms is where interactivity starts to be felt.
 * - **Inband FEC on.** Packet loss concealment matters more than raw bitrate
 *   here — a dropped video frame is invisible, a dropped audio frame is not.
 */

#include <libavcodec/avcodec.h>
#include <libavutil/channel_layout.h>
#include <libavutil/opt.h>
#include <libavutil/samplefmt.h>
#include <string.h>

struct DbOpusEncoder {
    AVCodecContext *ctx;
    AVFrame *frame;
    AVPacket *packet;
    /* Interleaved float samples not yet made into a whole frame. Opus only
     * encodes exact frame sizes, and a capture buffer is whatever the audio
     * engine felt like handing over. */
    float *pending;
    int pending_frames;
    int pending_cap;
};

struct DbOpusDecoder {
    AVCodecContext *ctx;
    AVFrame *frame;
    AVPacket *packet;
};

typedef struct DbOpusEncoder DbOpusEncoder;
typedef struct DbOpusDecoder DbOpusDecoder;

void db_opus_encoder_free(DbOpusEncoder *e);
void db_opus_decoder_free(DbOpusDecoder *d);

static void db_fail(char *err, int err_len, const char *msg)
{
    if (err && err_len > 0) {
        strncpy(err, msg, (size_t)err_len - 1);
        err[err_len - 1] = '\0';
    }
}

DbOpusEncoder *db_opus_encoder_new(int sample_rate, int channels, int bitrate,
                                   char *err, int err_len)
{
    /* Named rather than by id: ffmpeg also carries a native Opus encoder, and
     * libopus is the better one. */
    const AVCodec *codec = avcodec_find_encoder_by_name("libopus");
    if (!codec) {
        db_fail(err, err_len, "this ffmpeg build has no libopus encoder");
        return NULL;
    }

    DbOpusEncoder *e = av_mallocz(sizeof(DbOpusEncoder));
    if (!e) {
        db_fail(err, err_len, "out of memory");
        return NULL;
    }

    e->ctx = avcodec_alloc_context3(codec);
    if (!e->ctx) {
        db_fail(err, err_len, "could not allocate an encoder context");
        db_opus_encoder_free(e);
        return NULL;
    }

    e->ctx->sample_rate = sample_rate;
    e->ctx->bit_rate = bitrate;
    /* Interleaved float, which is exactly what WASAPI loopback produces, so the
     * capture buffer goes in without a conversion pass. */
    e->ctx->sample_fmt = AV_SAMPLE_FMT_FLT;
    av_channel_layout_default(&e->ctx->ch_layout, channels);
    e->ctx->flags |= AV_CODEC_FLAG_LOW_DELAY;

    av_opt_set(e->ctx->priv_data, "application", "audio", 0);
    av_opt_set(e->ctx->priv_data, "frame_duration", "10", 0);
    av_opt_set(e->ctx->priv_data, "fec", "1", 0);
    av_opt_set(e->ctx->priv_data, "vbr", "on", 0);
    /* How much loss the encoder should brace for. Without this, FEC is enabled
     * but carries nothing. */
    av_opt_set(e->ctx->priv_data, "packet_loss", "5", 0);

    if (avcodec_open2(e->ctx, codec, NULL) < 0) {
        db_fail(err, err_len, "libopus refused these parameters");
        db_opus_encoder_free(e);
        return NULL;
    }

    e->frame = av_frame_alloc();
    e->packet = av_packet_alloc();
    if (!e->frame || !e->packet) {
        db_fail(err, err_len, "out of memory");
        db_opus_encoder_free(e);
        return NULL;
    }

    e->frame->format = AV_SAMPLE_FMT_FLT;
    e->frame->sample_rate = sample_rate;
    e->frame->nb_samples = e->ctx->frame_size;
    if (av_channel_layout_copy(&e->frame->ch_layout, &e->ctx->ch_layout) < 0 ||
        av_frame_get_buffer(e->frame, 0) < 0) {
        db_fail(err, err_len, "could not allocate an audio frame");
        db_opus_encoder_free(e);
        return NULL;
    }
    return e;
}

/* Frames per Opus packet, which the caller needs in order to size buffers. */
int db_opus_frame_size(DbOpusEncoder *e)
{
    return (e && e->ctx) ? e->ctx->frame_size : 0;
}

/*
 * Feed interleaved float samples; emit zero or more whole Opus packets.
 *
 * Packets are written back to back into `out`, each preceded by a 2-byte
 * big-endian length, and the total byte count is returned. Framing them here
 * rather than returning one at a time keeps the Rust side a single call per
 * capture buffer, however many frames that buffer happened to span.
 *
 * Returns a negative ffmpeg error code on failure.
 */
int db_opus_encode(DbOpusEncoder *e, const float *samples, int frames,
                   unsigned char *out, int out_cap)
{
    if (!e || !e->ctx || !samples) {
        return -1;
    }
    const int channels = e->ctx->ch_layout.nb_channels;
    const int frame_size = e->ctx->frame_size;
    int written = 0;

    /* Accumulate, because Opus encodes exact frame sizes only. */
    int needed = (e->pending_frames + frames) * channels;
    if (e->pending_cap < needed) {
        float *grown = av_realloc(e->pending, (size_t)needed * sizeof(float));
        if (!grown) {
            return AVERROR(ENOMEM);
        }
        e->pending = grown;
        e->pending_cap = needed;
    }
    memcpy(e->pending + e->pending_frames * channels, samples,
           (size_t)frames * channels * sizeof(float));
    e->pending_frames += frames;

    int consumed = 0;
    while (e->pending_frames - consumed >= frame_size) {
        if (av_frame_make_writable(e->frame) < 0) {
            return -1;
        }
        memcpy(e->frame->data[0], e->pending + consumed * channels,
               (size_t)frame_size * channels * sizeof(float));
        consumed += frame_size;

        int rc = avcodec_send_frame(e->ctx, e->frame);
        if (rc < 0) {
            return rc;
        }
        while (1) {
            rc = avcodec_receive_packet(e->ctx, e->packet);
            if (rc == AVERROR(EAGAIN) || rc == AVERROR_EOF) {
                break;
            }
            if (rc < 0) {
                return rc;
            }
            if (written + 2 + e->packet->size > out_cap) {
                av_packet_unref(e->packet);
                return AVERROR(ENOSPC);
            }
            out[written] = (unsigned char)((e->packet->size >> 8) & 0xff);
            out[written + 1] = (unsigned char)(e->packet->size & 0xff);
            memcpy(out + written + 2, e->packet->data, (size_t)e->packet->size);
            written += 2 + e->packet->size;
            av_packet_unref(e->packet);
        }
    }

    /* Shuffle the remainder down rather than reallocating every call. */
    int left = e->pending_frames - consumed;
    if (left > 0 && consumed > 0) {
        memmove(e->pending, e->pending + consumed * channels,
                (size_t)left * channels * sizeof(float));
    }
    e->pending_frames = left;
    return written;
}

void db_opus_encoder_free(DbOpusEncoder *e)
{
    if (!e) {
        return;
    }
    if (e->frame) {
        av_frame_free(&e->frame);
    }
    if (e->packet) {
        av_packet_free(&e->packet);
    }
    if (e->ctx) {
        avcodec_free_context(&e->ctx);
    }
    if (e->pending) {
        av_free(e->pending);
    }
    av_free(e);
}

DbOpusDecoder *db_opus_decoder_new(int sample_rate, int channels, char *err,
                                   int err_len)
{
    const AVCodec *codec = avcodec_find_decoder(AV_CODEC_ID_OPUS);
    if (!codec) {
        db_fail(err, err_len, "this ffmpeg build has no Opus decoder");
        return NULL;
    }

    DbOpusDecoder *d = av_mallocz(sizeof(DbOpusDecoder));
    if (!d) {
        db_fail(err, err_len, "out of memory");
        return NULL;
    }

    d->ctx = avcodec_alloc_context3(codec);
    if (!d->ctx) {
        db_fail(err, err_len, "could not allocate a decoder context");
        db_opus_decoder_free(d);
        return NULL;
    }
    d->ctx->sample_rate = sample_rate;
    av_channel_layout_default(&d->ctx->ch_layout, channels);
    /* Ask for float out, matching what the render side wants to push at WASAPI. */
    d->ctx->request_sample_fmt = AV_SAMPLE_FMT_FLT;

    if (avcodec_open2(d->ctx, codec, NULL) < 0) {
        db_fail(err, err_len, "the Opus decoder refused these parameters");
        db_opus_decoder_free(d);
        return NULL;
    }

    d->frame = av_frame_alloc();
    d->packet = av_packet_alloc();
    if (!d->frame || !d->packet) {
        db_fail(err, err_len, "out of memory");
        db_opus_decoder_free(d);
        return NULL;
    }
    return d;
}

/*
 * Decode one Opus packet into interleaved float.
 *
 * Returns frames written, or a negative ffmpeg error code.
 *
 * `data` must be a real packet. Passing null here would mean end-of-stream to
 * libavcodec, not "conceal a loss" — it drains the decoder and breaks every
 * packet after it. Concealment is handled on the Rust side; see OpusDecoder.
 */
int db_opus_decode(DbOpusDecoder *d, const unsigned char *data, int len,
                   float *out, int out_cap_frames)
{
    if (!d || !d->ctx) {
        return -1;
    }
    d->packet->data = (unsigned char *)data;
    d->packet->size = len;
    int rc = avcodec_send_packet(d->ctx, d->packet);
    d->packet->data = NULL;
    d->packet->size = 0;
    if (rc < 0 && rc != AVERROR(EAGAIN)) {
        return rc;
    }

    int written = 0;
    const int channels = d->ctx->ch_layout.nb_channels;
    while (1) {
        rc = avcodec_receive_frame(d->ctx, d->frame);
        if (rc == AVERROR(EAGAIN) || rc == AVERROR_EOF) {
            break;
        }
        if (rc < 0) {
            return rc;
        }
        int frames = d->frame->nb_samples;
        if (written + frames > out_cap_frames) {
            av_frame_unref(d->frame);
            return AVERROR(ENOSPC);
        }
        if (d->frame->format == AV_SAMPLE_FMT_FLT) {
            memcpy(out + written * channels, d->frame->data[0],
                   (size_t)frames * channels * sizeof(float));
        } else if (d->frame->format == AV_SAMPLE_FMT_FLTP) {
            /* Planar: the decoder gives one buffer per channel and the render
             * side wants them woven together. */
            for (int f = 0; f < frames; f++) {
                for (int c = 0; c < channels; c++) {
                    out[(written + f) * channels + c] =
                        ((const float *)d->frame->data[c])[f];
                }
            }
        } else {
            av_frame_unref(d->frame);
            return -1;
        }
        written += frames;
        av_frame_unref(d->frame);
    }
    return written;
}

void db_opus_decoder_free(DbOpusDecoder *d)
{
    if (!d) {
        return;
    }
    if (d->frame) {
        av_frame_free(&d->frame);
    }
    if (d->packet) {
        av_packet_free(&d->packet);
    }
    if (d->ctx) {
        avcodec_free_context(&d->ctx);
    }
    av_free(d);
}
