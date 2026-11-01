#!/usr/bin/env bash
#
# Build the static FFmpeg the Linux binary links, into third_party/ffmpeg-linux.
#
# # What it builds
#
# The same FFmpeg 7.1.5 and libopus 1.6.1 as scripts/build-ffmpeg.sh, pinned to
# the same archives, configured with `--disable-everything` and then only what
# tain-core/ffmpeg/db_vaapi.c and db_audio.c call:
#
#   - The h264, hevc and av1 decoders, with their VA-API hwaccels.
#   - The h264_vaapi, hevc_vaapi and av1_vaapi encoders.
#   - The native opus decoder (which pulls in swresample) and the libopus
#     encoder, with libopus built here.
#
# Static archives built position-independent, because tain-core/build.rs links
# them into libtain-media.so, which the binary opens at run time. libva is the
# only library they need and is not linked: see build.rs for why.
#
# # Why it is not committed, unlike the Windows DLLs
#
# A static archive carries the glibc it was compiled against into whatever
# links it, so these have to be built where the release is linked — the
# release job's ubuntu:22.04 container — or the binary asks for a newer glibc
# than it promises. The release job runs this, caches the result, and builds
# against it. On a developer machine, run it once; the result is gitignored.
#
# # How to run it
#
# On a glibc Linux with a C compiler, make, nasm, pkg-config, curl and the libva
# development headers:
#
#     sudo apt install build-essential nasm pkg-config curl libva-dev
#     sudo dnf install gcc make nasm pkgconf curl libva-devel
#
# then, from the repository root:
#
#     ./scripts/build-ffmpeg-linux.sh [work-dir]
#
# Without it, a Linux build links the system's own FFmpeg if there is one,
# which is fine for working on Tain; see tain-core/build.rs.

set -euo pipefail

FFMPEG_VERSION=7.1.5
# The same archive and digest as scripts/build-ffmpeg.sh, checked against
# ffmpeg-7.1.5.tar.xz.asc and the FFmpeg release key when that was pinned.
FFMPEG_SHA256=de668509caf9e35e3cd162473441fdb29538c6d96ed080292b3cf9e6fc5d558f
OPUS_VERSION=1.6.1
OPUS_SHA256=6ffcb593207be92584df15b32466ed64bbec99109f007c82205f0194572411a1

die() {
    echo "build-ffmpeg-linux: $*" >&2
    exit 1
}

[ "$(uname -s)" = Linux ] || die "this builds the Linux libraries; Windows has scripts/build-ffmpeg.sh"
for tool in gcc make nasm pkg-config curl tar sha256sum; do
    command -v "$tool" >/dev/null || die "$tool is missing; see the install line at the top of this script"
done
# pkg-config reports libva's VA-API version: libva 2.14 is VA-API 1.14, the
# first with the AV1 encoding interface av1_vaapi is built on.
pkg-config --exists 'libva >= 1.14' libva-drm ||
    die "the libva development headers are missing, or older than libva 2.14 (libva-dev, libva-devel)"

root=$(cd "$(dirname "$0")/.." && pwd)
work=${1:-${TMPDIR:-/tmp}/tain-ffmpeg-linux-build}
dest=$root/third_party/ffmpeg-linux
jobs=$(nproc)
mkdir -p "$work"

fetch() {
    local url=$1 file=$2 sha=$3
    if [ ! -f "$work/$file" ]; then
        curl -fL --retry 5 -o "$work/$file.part" "$url"
        mv "$work/$file.part" "$work/$file"
    fi
    echo "$sha  $work/$file" | sha256sum -c - || die "$file is not the pinned archive"
}

fetch "https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz" \
    "ffmpeg-$FFMPEG_VERSION.tar.xz" "$FFMPEG_SHA256"
fetch "https://downloads.xiph.org/releases/opus/opus-$OPUS_VERSION.tar.gz" \
    "opus-$OPUS_VERSION.tar.gz" "$OPUS_SHA256"

rm -rf "$work/ffmpeg-$FFMPEG_VERSION" "$work/opus-$OPUS_VERSION" "$work/deps" "$work/stage"
tar -xf "$work/ffmpeg-$FFMPEG_VERSION.tar.xz" -C "$work"
tar -xf "$work/opus-$OPUS_VERSION.tar.gz" -C "$work"

# ── libopus, static and position-independent ────────────────────────────────
(
    cd "$work/opus-$OPUS_VERSION"
    ./configure \
        --prefix="$work/deps" \
        --disable-shared \
        --enable-static \
        --with-pic \
        --disable-doc \
        --disable-extra-programs
    make -j"$jobs"
    make install
)

# ── FFmpeg ───────────────────────────────────────────────────────────────────
#
# The Windows configure line with the Windows parts swapped out: pthreads for
# Win32 threads, VA-API for D3D11VA, and static archives built with -fPIC for
# DLLs. --disable-autodetect turns off thread and hwaccel detection too, hence
# the explicit --enable-pthreads and --enable-vaapi. VA-API reaches the GPU
# through its DRM render node, so neither Xlib nor libdrm is wanted.
ffmpeg_configure=(
    --prefix=/opt/ffmpeg
    --target-os=linux
    --arch=x86_64
    --pkg-config-flags=--static
    --disable-shared
    --enable-static
    --enable-pic
    --enable-version3
    --disable-debug
    --disable-programs
    --disable-doc
    --disable-autodetect
    --disable-everything
    --disable-avdevice
    --disable-avformat
    --disable-avfilter
    --disable-swscale
    --enable-pthreads
    --enable-vaapi
    --enable-libopus
    --enable-decoder=h264,hevc,av1,opus
    --enable-encoder=libopus,h264_vaapi,hevc_vaapi,av1_vaapi
    --enable-hwaccel=h264_vaapi,hevc_vaapi,av1_vaapi
)
(
    cd "$work/ffmpeg-$FFMPEG_VERSION"
    PKG_CONFIG_PATH="$work/deps/lib/pkgconfig" ./configure "${ffmpeg_configure[@]}"
    make -j"$jobs"
    make install DESTDIR="$work/stage"
)
staged=$work/stage/opt/ffmpeg

# Every component asked for has to be in the result: configure drops a
# component it cannot build without failing, and av1_vaapi in particular needs
# libva 2.14 or later to be there at all.
for want in h264_vaapi_hwaccel hevc_vaapi_hwaccel av1_vaapi_hwaccel \
    h264_vaapi_encoder hevc_vaapi_encoder av1_vaapi_encoder libopus_encoder opus_decoder; do
    grep -q "^#define CONFIG_${want^^} 1" "$work/ffmpeg-$FFMPEG_VERSION/config_components.h" ||
        die "configure left out $want; nothing has been copied"
done

# ── Into the repository ─────────────────────────────────────────────────────
rm -rf "$dest"
mkdir -p "$dest/lib" "$dest/include"
for lib in libavcodec libavutil libswresample; do
    cp "$staged/lib/$lib.a" "$dest/lib/"
    cp -r "$staged/include/$lib" "$dest/include/"
done
cp "$work/deps/lib/libopus.a" "$dest/lib/"
src=$work/ffmpeg-$FFMPEG_VERSION
cp "$src/LICENSE.md" "$src/COPYING.LGPLv3" "$src/COPYING.GPLv3" "$dest/"
cp "$work/opus-$OPUS_VERSION/COPYING" "$dest/COPYING.libopus"
{
    echo "FFmpeg $FFMPEG_VERSION ($FFMPEG_SHA256)"
    echo "libopus $OPUS_VERSION ($OPUS_SHA256)"
    echo "gcc $(gcc -dumpversion), $(ldd --version | head -n1)"
    echo "libva $(pkg-config --modversion libva)"
    echo
    echo "configure ${ffmpeg_configure[*]}"
} > "$dest/BUILD.txt"

cat "$dest/BUILD.txt"
echo
ls -l "$dest/lib"
