/*
 * The screen, from the PipeWire stream a screen cast portal hands over.
 *
 * # Why C, and why libpipewire is opened rather than linked
 *
 * PipeWire's format negotiation is built from SPA pods, whose builders are
 * inline functions and macros in the headers: from C they are a few lines,
 * and from Rust they are a bindgen run, which would make libclang a build
 * prerequisite — the reason the FFmpeg shims are C too.
 *
 * libpipewire itself is opened with dlopen and every pw_* function called
 * through a pointer, so the binary links nothing new: a machine without
 * PipeWire still runs Tain, and only screen capture is unavailable. Nothing
 * here may call a pw_* function by name, or the link would need -lpipewire.
 *
 * # What it does
 *
 * Connects to the portal's PipeWire remote, or with no remote to the user's
 * own PipeWire daemon (where KWin publishes the window streams it is asked
 * for directly), consumes one video node in shared
 * memory (no DMA-BUF modifiers are offered, so every compositor falls back to
 * memory it maps for us), and keeps the newest frame as tightly packed BGRA,
 * converting from whichever of BGRx, BGRA, RGBx or RGBA was negotiated.
 * A frame is copied out under a lock, so a reader never sees half of one.
 */

#include <dlfcn.h>
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include <pipewire/pipewire.h>
#include <spa/param/video/format-utils.h>

/* ── libpipewire, opened at run time ─────────────────────────────────────── */

static struct {
    void (*init)(int *argc, char **argv[]);
    struct pw_thread_loop *(*thread_loop_new)(const char *name, const struct spa_dict *props);
    int (*thread_loop_start)(struct pw_thread_loop *loop);
    void (*thread_loop_stop)(struct pw_thread_loop *loop);
    void (*thread_loop_destroy)(struct pw_thread_loop *loop);
    void (*thread_loop_lock)(struct pw_thread_loop *loop);
    void (*thread_loop_unlock)(struct pw_thread_loop *loop);
    struct pw_loop *(*thread_loop_get_loop)(struct pw_thread_loop *loop);
    struct pw_context *(*context_new)(struct pw_loop *loop, struct pw_properties *props, size_t user_data_size);
    void (*context_destroy)(struct pw_context *context);
    struct pw_core *(*context_connect_fd)(struct pw_context *context, int fd,
                                          struct pw_properties *props, size_t user_data_size);
    struct pw_core *(*context_connect)(struct pw_context *context, struct pw_properties *props,
                                       size_t user_data_size);
    int (*core_disconnect)(struct pw_core *core);
    struct pw_properties *(*properties_new)(const char *key, ...);
    struct pw_stream *(*stream_new)(struct pw_core *core, const char *name, struct pw_properties *props);
    void (*stream_add_listener)(struct pw_stream *stream, struct spa_hook *listener,
                                const struct pw_stream_events *events, void *data);
    int (*stream_connect)(struct pw_stream *stream, enum pw_direction direction, uint32_t target_id,
                          enum pw_stream_flags flags, const struct spa_pod **params, uint32_t n_params);
    int (*stream_update_params)(struct pw_stream *stream, const struct spa_pod **params, uint32_t n_params);
    struct pw_buffer *(*stream_dequeue_buffer)(struct pw_stream *stream);
    int (*stream_queue_buffer)(struct pw_stream *stream, struct pw_buffer *buffer);
    int (*stream_disconnect)(struct pw_stream *stream);
    void (*stream_destroy)(struct pw_stream *stream);
} pw;

static pthread_once_t pw_once = PTHREAD_ONCE_INIT;
static char pw_error[256];
static int pw_loaded;

static void db_pw_load_once(void)
{
    void *lib = dlopen("libpipewire-0.3.so.0", RTLD_NOW | RTLD_LOCAL);
    if (!lib) {
        snprintf(pw_error, sizeof pw_error, "PipeWire is not installed (%s)", dlerror());
        return;
    }
#define LOAD(field, name)                                                              \
    do {                                                                               \
        *(void **)&pw.field = dlsym(lib, name);                                        \
        if (!pw.field) {                                                               \
            snprintf(pw_error, sizeof pw_error, "libpipewire-0.3 has no %s", name);    \
            return;                                                                    \
        }                                                                              \
    } while (0)
    LOAD(init, "pw_init");
    LOAD(thread_loop_new, "pw_thread_loop_new");
    LOAD(thread_loop_start, "pw_thread_loop_start");
    LOAD(thread_loop_stop, "pw_thread_loop_stop");
    LOAD(thread_loop_destroy, "pw_thread_loop_destroy");
    LOAD(thread_loop_lock, "pw_thread_loop_lock");
    LOAD(thread_loop_unlock, "pw_thread_loop_unlock");
    LOAD(thread_loop_get_loop, "pw_thread_loop_get_loop");
    LOAD(context_new, "pw_context_new");
    LOAD(context_destroy, "pw_context_destroy");
    LOAD(context_connect_fd, "pw_context_connect_fd");
    LOAD(context_connect, "pw_context_connect");
    LOAD(core_disconnect, "pw_core_disconnect");
    LOAD(properties_new, "pw_properties_new");
    LOAD(stream_new, "pw_stream_new");
    LOAD(stream_add_listener, "pw_stream_add_listener");
    LOAD(stream_connect, "pw_stream_connect");
    LOAD(stream_update_params, "pw_stream_update_params");
    LOAD(stream_dequeue_buffer, "pw_stream_dequeue_buffer");
    LOAD(stream_queue_buffer, "pw_stream_queue_buffer");
    LOAD(stream_disconnect, "pw_stream_disconnect");
    LOAD(stream_destroy, "pw_stream_destroy");
#undef LOAD
    pw.init(NULL, NULL);
    pw_loaded = 1;
}

static void db_fail(char *err, int err_len, const char *msg)
{
    if (err && err_len > 0) {
        strncpy(err, msg, (size_t)err_len - 1);
        err[err_len - 1] = '\0';
    }
}

/* Whether libpipewire could be opened; *err says why not. */
int db_pw_available(char *err, int err_len)
{
    pthread_once(&pw_once, db_pw_load_once);
    if (!pw_loaded) {
        db_fail(err, err_len, pw_error);
    }
    return pw_loaded;
}

/* ── One stream ─────────────────────────────────────────────────────────── */

struct DbPwStream {
    struct pw_thread_loop *loop;
    struct pw_context *context;
    struct pw_core *core;
    struct pw_stream *stream;
    struct spa_hook listener;

    pthread_mutex_t mutex;
    pthread_cond_t cond;
    /* Guarded by mutex. */
    uint32_t format;
    int width, height;
    unsigned char *frame;
    size_t frame_cap;
    int frame_width, frame_height;
    int fresh;
    int ended;
    char error[256];
};

typedef struct DbPwStream DbPwStream;

/* Copy one row into BGRA, whatever it was negotiated as. */
static void db_row(unsigned char *dst, const unsigned char *src, int width, uint32_t format)
{
    switch (format) {
    case SPA_VIDEO_FORMAT_BGRA:
        memcpy(dst, src, (size_t)width * 4);
        break;
    case SPA_VIDEO_FORMAT_BGRx:
        for (int x = 0; x < width; x++) {
            dst[x * 4 + 0] = src[x * 4 + 0];
            dst[x * 4 + 1] = src[x * 4 + 1];
            dst[x * 4 + 2] = src[x * 4 + 2];
            dst[x * 4 + 3] = 255;
        }
        break;
    case SPA_VIDEO_FORMAT_RGBA:
    case SPA_VIDEO_FORMAT_RGBx:
        for (int x = 0; x < width; x++) {
            dst[x * 4 + 0] = src[x * 4 + 2];
            dst[x * 4 + 1] = src[x * 4 + 1];
            dst[x * 4 + 2] = src[x * 4 + 0];
            dst[x * 4 + 3] = format == SPA_VIDEO_FORMAT_RGBA ? src[x * 4 + 3] : 255;
        }
        break;
    default:
        memset(dst, 0, (size_t)width * 4);
        break;
    }
}

static void on_state_changed(void *data, enum pw_stream_state old, enum pw_stream_state state,
                             const char *error)
{
    (void)old;
    DbPwStream *s = data;
    if (state == PW_STREAM_STATE_ERROR || state == PW_STREAM_STATE_UNCONNECTED) {
        pthread_mutex_lock(&s->mutex);
        s->ended = 1;
        snprintf(s->error, sizeof s->error, "%s",
                 error ? error : "the screen's stream was disconnected");
        pthread_cond_broadcast(&s->cond);
        pthread_mutex_unlock(&s->mutex);
    }
}

static void on_param_changed(void *data, uint32_t id, const struct spa_pod *param)
{
    DbPwStream *s = data;
    if (!param || id != SPA_PARAM_Format) {
        return;
    }
    uint32_t media_type, media_subtype;
    if (spa_format_parse(param, &media_type, &media_subtype) < 0 ||
        media_type != SPA_MEDIA_TYPE_video || media_subtype != SPA_MEDIA_SUBTYPE_raw) {
        return;
    }
    struct spa_video_info_raw info;
    memset(&info, 0, sizeof info);
    if (spa_format_video_raw_parse(param, &info) < 0) {
        return;
    }

    pthread_mutex_lock(&s->mutex);
    s->format = info.format;
    s->width = (int)info.size.width;
    s->height = (int)info.size.height;
    pthread_cond_broadcast(&s->cond);
    pthread_mutex_unlock(&s->mutex);

    /* Memory we can read, a stride of whole pixels, and the crop metadata a
     * window capture uses to say which part of the buffer is the window. */
    uint8_t buffer[1024];
    struct spa_pod_builder b = SPA_POD_BUILDER_INIT(buffer, sizeof buffer);
    int stride = (int)info.size.width * 4;
    const struct spa_pod *params[3];
    params[0] = spa_pod_builder_add_object(&b,
        SPA_TYPE_OBJECT_ParamBuffers, SPA_PARAM_Buffers,
        SPA_PARAM_BUFFERS_buffers, SPA_POD_CHOICE_RANGE_Int(4, 2, 8),
        SPA_PARAM_BUFFERS_blocks, SPA_POD_Int(1),
        SPA_PARAM_BUFFERS_size, SPA_POD_Int(stride * (int)info.size.height),
        SPA_PARAM_BUFFERS_stride, SPA_POD_Int(stride),
        SPA_PARAM_BUFFERS_dataType, SPA_POD_CHOICE_FLAGS_Int((1 << SPA_DATA_MemPtr) | (1 << SPA_DATA_MemFd)));
    params[1] = spa_pod_builder_add_object(&b,
        SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta,
        SPA_PARAM_META_type, SPA_POD_Id(SPA_META_Header),
        SPA_PARAM_META_size, SPA_POD_Int(sizeof(struct spa_meta_header)));
    params[2] = spa_pod_builder_add_object(&b,
        SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta,
        SPA_PARAM_META_type, SPA_POD_Id(SPA_META_VideoCrop),
        SPA_PARAM_META_size, SPA_POD_Int(sizeof(struct spa_meta_region)));
    pw.stream_update_params(s->stream, params, 3);
}

static void on_process(void *data)
{
    DbPwStream *s = data;
    /* Only the newest buffer matters; older ones go straight back. */
    struct pw_buffer *latest = NULL, *next;
    while ((next = pw.stream_dequeue_buffer(s->stream)) != NULL) {
        if (latest) {
            pw.stream_queue_buffer(s->stream, latest);
        }
        latest = next;
    }
    if (!latest) {
        return;
    }

    struct spa_buffer *buf = latest->buffer;
    struct spa_data *d = &buf->datas[0];
    struct spa_meta_header *header = spa_buffer_find_meta_data(buf, SPA_META_Header, sizeof *header);
    int corrupt = header && (header->flags & SPA_META_HEADER_FLAG_CORRUPTED);
    if (!d->data || !d->chunk || d->chunk->size == 0 || corrupt) {
        pw.stream_queue_buffer(s->stream, latest);
        return;
    }

    pthread_mutex_lock(&s->mutex);
    int width = s->width, height = s->height;
    uint32_t format = s->format;
    int x0 = 0, y0 = 0;
    struct spa_meta_region *crop = spa_buffer_find_meta_data(buf, SPA_META_VideoCrop, sizeof *crop);
    if (crop && spa_meta_region_is_valid(crop) &&
        crop->region.position.x >= 0 && crop->region.position.y >= 0 &&
        crop->region.position.x + (int)crop->region.size.width <= width &&
        crop->region.position.y + (int)crop->region.size.height <= height) {
        x0 = crop->region.position.x;
        y0 = crop->region.position.y;
        width = (int)crop->region.size.width;
        height = (int)crop->region.size.height;
    }
    int stride = d->chunk->stride > 0 ? d->chunk->stride : s->width * 4;
    const unsigned char *src = (const unsigned char *)d->data + d->chunk->offset;
    size_t available = d->chunk->size;
    size_t needed = (size_t)width * (size_t)height * 4;

    if (width > 0 && height > 0 &&
        (size_t)stride * (size_t)(y0 + height - 1) + (size_t)(x0 + width) * 4 <= available) {
        if (s->frame_cap < needed) {
            unsigned char *grown = realloc(s->frame, needed);
            if (grown) {
                s->frame = grown;
                s->frame_cap = needed;
            }
        }
        if (s->frame_cap >= needed) {
            for (int y = 0; y < height; y++) {
                db_row(s->frame + (size_t)y * (size_t)width * 4,
                       src + (size_t)(y0 + y) * (size_t)stride + (size_t)x0 * 4, width, format);
            }
            s->frame_width = width;
            s->frame_height = height;
            s->fresh = 1;
            pthread_cond_broadcast(&s->cond);
        }
    }
    pthread_mutex_unlock(&s->mutex);
    pw.stream_queue_buffer(s->stream, latest);
}

static const struct pw_stream_events stream_events = {
    PW_VERSION_STREAM_EVENTS,
    .state_changed = on_state_changed,
    .param_changed = on_param_changed,
    .process = on_process,
};

void db_pw_stream_close(DbPwStream *s);

/*
 * Connect to `node` on the PipeWire remote `fd` (taken over: PipeWire closes
 * it), or on the user's own daemon when `fd` is negative, and start receiving
 * frames.
 */
DbPwStream *db_pw_stream_open(int fd, unsigned int node, char *err, int err_len)
{
    if (!db_pw_available(err, err_len)) {
        return NULL;
    }
    DbPwStream *s = calloc(1, sizeof *s);
    if (!s) {
        db_fail(err, err_len, "out of memory");
        return NULL;
    }
    pthread_mutex_init(&s->mutex, NULL);
    pthread_condattr_t attr;
    pthread_condattr_init(&attr);
    pthread_condattr_setclock(&attr, CLOCK_MONOTONIC);
    pthread_cond_init(&s->cond, &attr);
    pthread_condattr_destroy(&attr);

    s->loop = pw.thread_loop_new("tain-capture", NULL);
    if (!s->loop) {
        db_fail(err, err_len, "could not start PipeWire's thread");
        db_pw_stream_close(s);
        return NULL;
    }
    s->context = pw.context_new(pw.thread_loop_get_loop(s->loop), NULL, 0);
    if (!s->context) {
        db_fail(err, err_len, "could not create a PipeWire context");
        db_pw_stream_close(s);
        return NULL;
    }
    if (pw.thread_loop_start(s->loop) < 0) {
        db_fail(err, err_len, "could not start PipeWire's thread");
        db_pw_stream_close(s);
        return NULL;
    }

    pw.thread_loop_lock(s->loop);
    s->core = fd < 0 ? pw.context_connect(s->context, NULL, 0)
                     : pw.context_connect_fd(s->context, fd, NULL, 0);
    if (!s->core) {
        pw.thread_loop_unlock(s->loop);
        db_fail(err, err_len, "could not connect to the screen's PipeWire remote");
        db_pw_stream_close(s);
        return NULL;
    }
    s->stream = pw.stream_new(s->core, "Tain screen capture",
                              pw.properties_new(PW_KEY_MEDIA_TYPE, "Video",
                                                PW_KEY_MEDIA_CATEGORY, "Capture",
                                                PW_KEY_MEDIA_ROLE, "Screen", NULL));
    if (!s->stream) {
        pw.thread_loop_unlock(s->loop);
        db_fail(err, err_len, "could not create a PipeWire stream");
        db_pw_stream_close(s);
        return NULL;
    }
    pw.stream_add_listener(s->stream, &s->listener, &stream_events, s);

    uint8_t buffer[1024];
    struct spa_pod_builder b = SPA_POD_BUILDER_INIT(buffer, sizeof buffer);
    const struct spa_pod *params[1];
    params[0] = spa_pod_builder_add_object(&b,
        SPA_TYPE_OBJECT_Format, SPA_PARAM_EnumFormat,
        SPA_FORMAT_mediaType, SPA_POD_Id(SPA_MEDIA_TYPE_video),
        SPA_FORMAT_mediaSubtype, SPA_POD_Id(SPA_MEDIA_SUBTYPE_raw),
        SPA_FORMAT_VIDEO_format, SPA_POD_CHOICE_ENUM_Id(5,
            SPA_VIDEO_FORMAT_BGRx, SPA_VIDEO_FORMAT_BGRx, SPA_VIDEO_FORMAT_BGRA,
            SPA_VIDEO_FORMAT_RGBx, SPA_VIDEO_FORMAT_RGBA),
        SPA_FORMAT_VIDEO_size, SPA_POD_CHOICE_RANGE_Rectangle(
            &SPA_RECTANGLE(1920, 1080), &SPA_RECTANGLE(1, 1), &SPA_RECTANGLE(16384, 16384)),
        SPA_FORMAT_VIDEO_framerate, SPA_POD_CHOICE_RANGE_Fraction(
            &SPA_FRACTION(60, 1), &SPA_FRACTION(0, 1), &SPA_FRACTION(1000, 1)));

    int rc = pw.stream_connect(s->stream, PW_DIRECTION_INPUT, node,
                               PW_STREAM_FLAG_AUTOCONNECT | PW_STREAM_FLAG_MAP_BUFFERS,
                               params, 1);
    pw.thread_loop_unlock(s->loop);
    if (rc < 0) {
        char msg[128];
        snprintf(msg, sizeof msg, "could not connect to the screen's stream (%s)", strerror(-rc));
        db_fail(err, err_len, msg);
        db_pw_stream_close(s);
        return NULL;
    }
    return s;
}

static void db_deadline(struct timespec *deadline, int timeout_ms)
{
    clock_gettime(CLOCK_MONOTONIC, deadline);
    deadline->tv_sec += timeout_ms / 1000;
    deadline->tv_nsec += (long)(timeout_ms % 1000) * 1000000L;
    if (deadline->tv_nsec >= 1000000000L) {
        deadline->tv_sec += 1;
        deadline->tv_nsec -= 1000000000L;
    }
}

/*
 * Wait up to `timeout_ms` for the negotiated size. 1 when known, 0 on
 * timeout, -1 when the stream ended (`err` says why).
 */
int db_pw_stream_wait_size(DbPwStream *s, int timeout_ms, int *width, int *height,
                           char *err, int err_len)
{
    struct timespec deadline;
    db_deadline(&deadline, timeout_ms);
    pthread_mutex_lock(&s->mutex);
    while (s->width <= 0 && !s->ended) {
        if (pthread_cond_timedwait(&s->cond, &s->mutex, &deadline) == ETIMEDOUT) {
            break;
        }
    }
    int rc = s->width > 0 ? 1 : (s->ended ? -1 : 0);
    *width = s->width;
    *height = s->height;
    if (rc < 0) {
        db_fail(err, err_len, s->error);
    }
    pthread_mutex_unlock(&s->mutex);
    return rc;
}

/*
 * Wait up to `timeout_ms` for a frame newer than the last one taken, and copy
 * it into `dst` as tightly packed BGRA.
 *
 * 1 with a frame, 0 when none came in time, -1 when the stream ended (`err`
 * says why), and -2 when `dst` is too small — *width and *height then say
 * what it needs, and the frame is kept for the next call.
 */
int db_pw_stream_frame(DbPwStream *s, int timeout_ms, unsigned char *dst, size_t dst_cap,
                       int *width, int *height, char *err, int err_len)
{
    struct timespec deadline;
    db_deadline(&deadline, timeout_ms);
    pthread_mutex_lock(&s->mutex);
    while (!s->fresh && !s->ended) {
        if (pthread_cond_timedwait(&s->cond, &s->mutex, &deadline) == ETIMEDOUT) {
            break;
        }
    }
    int rc;
    if (s->fresh) {
        size_t needed = (size_t)s->frame_width * (size_t)s->frame_height * 4;
        *width = s->frame_width;
        *height = s->frame_height;
        if (dst_cap < needed) {
            rc = -2;
        } else {
            memcpy(dst, s->frame, needed);
            s->fresh = 0;
            rc = 1;
        }
    } else if (s->ended) {
        db_fail(err, err_len, s->error);
        rc = -1;
    } else {
        rc = 0;
    }
    pthread_mutex_unlock(&s->mutex);
    return rc;
}

void db_pw_stream_close(DbPwStream *s)
{
    if (!s) {
        return;
    }
    if (s->loop) {
        pw.thread_loop_lock(s->loop);
        if (s->stream) {
            spa_hook_remove(&s->listener);
            pw.stream_disconnect(s->stream);
            pw.stream_destroy(s->stream);
        }
        if (s->core) {
            pw.core_disconnect(s->core);
        }
        pw.thread_loop_unlock(s->loop);
        pw.thread_loop_stop(s->loop);
    }
    if (s->context) {
        pw.context_destroy(s->context);
    }
    if (s->loop) {
        pw.thread_loop_destroy(s->loop);
    }
    pthread_cond_destroy(&s->cond);
    pthread_mutex_destroy(&s->mutex);
    free(s->frame);
    free(s);
}
