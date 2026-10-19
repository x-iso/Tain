#!/usr/bin/env bash
#
# Build the FFmpeg libraries Tain ships, from source, into third_party/ffmpeg.
#
# # What it builds
#
# avcodec-61, avutil-59 and swresample-5 (FFmpeg 7.1's ABI) with the headers
# that go with them, configured with `--disable-everything` and then only what
# `tain-core/ffmpeg/db_decode.c` and `db_audio.c` call:
#
#   - The h264, hevc and av1 decoders, with their D3D11VA hwaccels. The
#     `d3d11va2` ones are those used: they output AV_PIX_FMT_D3D11, the format
#     the shim's get_format asks for and the only one it accepts. Their legacy
#     `d3d11va` twins come along; see the configure line for why.
#   - The native opus decoder, which is what pulls in swresample.
#   - The libopus encoder, with libopus built here and linked into avcodec
#     statically, so it is not a fourth DLL.
#
# No parsers: the shim hands libavcodec whole access units and never calls
# av_parser_*. The one bitstream filter, av1_frame_split, belongs to the AV1
# decoder, and configure selects it along with it.
#
# The general-purpose build this replaces had about forty other libraries in
# avcodec and was 66 MB on its own.
#
# # How to run it
#
# In an MSYS2 UCRT64 shell (https://www.msys2.org), with:
#
#     pacman -S --needed make diffutils mingw-w64-ucrt-x86_64-gcc \
#         mingw-w64-ucrt-x86_64-nasm mingw-w64-ucrt-x86_64-pkgconf
#
# then, from the repository root:
#
#     ./scripts/build-ffmpeg.sh [work-dir]
#
# or from PowerShell:
#
#     C:\msys64\msys2_shell.cmd -ucrt64 -defterm -no-start -here -c ./scripts/build-ffmpeg.sh
#
# UCRT64 rather than MINGW64, so the DLLs link the Universal CRT that Windows 10
# and later carry instead of the old msvcrt.dll. The build it replaces did the
# same.
#
# It downloads two source archives, checks each against the SHA-256 pinned
# below, builds, and replaces everything in third_party/ffmpeg except
# PROVENANCE.md. Update that by hand from what this prints at the end.
#
# # Afterwards
#
# Regenerate licenses/ffmpeg-configuration.txt from the configure line printed
# at the end, and see licenses/README.md for the rest.

set -euo pipefail

# The time binutils writes wherever it would write the time of the build: the
# PE header that `strip` rewrites when FFmpeg installs, and archive members. ld
# is told separately, with --no-insert-timestamp below. With both, the same
# toolchain building the same sources produces the same bytes.
export SOURCE_DATE_EPOCH=0

# FFmpeg 7.1 is the ABI Tain links. 8.x carries avcodec-62 and later, and moving
# up is a change to make deliberately, with the shim, not by bumping this.
FFMPEG_VERSION=7.1.5
# Checked against ffmpeg-7.1.5.tar.xz.asc, signed by the FFmpeg release key
# (FCF9 86EA 15E6 E293 A564 4F10 B432 2F04 D676 58D8), when this was pinned.
FFMPEG_SHA256=de668509caf9e35e3cd162473441fdb29538c6d96ed080292b3cf9e6fc5d558f

OPUS_VERSION=1.6.1
# The digest xiph publishes in https://downloads.xiph.org/releases/opus/SHA256SUMS.txt.
# Their release tarballs stop at 1.5.2 on GitHub, so this one comes from xiph.
OPUS_SHA256=6ffcb593207be92584df15b32466ed64bbec99109f007c82205f0194572411a1

die() {
    echo "build-ffmpeg: $*" >&2
    exit 1
}

[ "${MSYSTEM:-}" = UCRT64 ] || die "run this from an MSYS2 UCRT64 shell (MSYSTEM is '${MSYSTEM:-unset}')"
for tool in gcc make nasm pkg-config curl tar sha256sum; do
    command -v "$tool" >/dev/null || die "$tool is missing; see the pacman line at the top of this script"
done

root=$(cd "$(dirname "$0")/.." && pwd)
work=${1:-${TMPDIR:-/tmp}/tain-ffmpeg-build}
dest=$root/third_party/ffmpeg
jobs=$(nproc)
mkdir -p "$work"

# Download once, then verify whatever is there, so a stale or tampered archive
# in the work directory fails the same way a bad download does.
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

# Fresh trees every time: a half-configured one from an earlier attempt is the
# kind of state that makes two runs of the same script disagree.
rm -rf "$work/ffmpeg-$FFMPEG_VERSION" "$work/opus-$OPUS_VERSION" "$work/deps" "$work/stage"
tar -xf "$work/ffmpeg-$FFMPEG_VERSION.tar.xz" -C "$work"
tar -xf "$work/opus-$OPUS_VERSION.tar.gz" -C "$work"

# ── libopus, static, for avcodec to link in ─────────────────────────────────
(
    cd "$work/opus-$OPUS_VERSION"
    ./configure \
        --prefix="$work/deps" \
        --disable-shared \
        --enable-static \
        --disable-doc \
        --disable-extra-programs
    make -j"$jobs"
    make install
)

# ── FFmpeg ───────────────────────────────────────────────────────────────────
#
# --enable-version3 keeps the licence where the build this replaced had it,
# LGPL-3.0-or-later, so the notices did not change with it. Nothing in here
# needs it; without it this is LGPL-2.1-or-later.
#
# --disable-autodetect, so nothing is linked because it happened to be installed
# on the machine that built it. It turns off thread detection too, hence the
# explicit --enable-w32threads (Win32 threads rather than pthreads, so there is
# no winpthread to carry) and --enable-d3d11va.
#
# The prefix is a placeholder. It is installed under a staging directory, and
# the configure line is compiled into every DLL, where a path from the machine
# that built it has no business being.
ffmpeg_configure=(
    --prefix=/opt/ffmpeg
    --target-os=mingw32
    --arch=x86_64
    --pkg-config-flags=--static
    --enable-shared
    --disable-static
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
    --enable-w32threads
    --enable-d3d11va
    --enable-libopus
    --enable-decoder=h264,hevc,av1,opus
    --enable-encoder=libopus
    # The d3d11va2 hwaccels are the ones used. Each lives in the same file as
    # its legacy d3d11va twin, and libavcodec's Makefile only compiles that file
    # for the legacy one; asking for d3d11va2 alone fails to link. The twins are
    # a few hundred bytes of descriptor over shared code.
    --enable-hwaccel=h264_d3d11va,h264_d3d11va2,hevc_d3d11va,hevc_d3d11va2,av1_d3d11va,av1_d3d11va2
    # libgcc inside the DLLs rather than beside them.
    #
    # -no-pthread, because MSYS2's gcc links winpthreads into everything by
    # default, and configure then finds clock_gettime and nanosleep there:
    # avutil came out importing libwinpthread-1.dll, which no Windows machine
    # has. Without it FFmpeg takes the fallbacks an MSVC build takes, and the
    # one call that matters to Tain, the 2 ms wait in dxva2.c when the GPU
    # reports E_PENDING, sleeps through usleep instead.
    #
    # No PE timestamp from the linker; see SOURCE_DATE_EPOCH at the top.
    "--extra-ldflags=-static-libgcc -no-pthread -Wl,--no-insert-timestamp"
)
(
    cd "$work/ffmpeg-$FFMPEG_VERSION"
    PKG_CONFIG_PATH="$work/deps/lib/pkgconfig" ./configure "${ffmpeg_configure[@]}"
    make -j"$jobs"
    make install DESTDIR="$work/stage"
)
staged=$work/stage/opt/ffmpeg

# The DLLs are embedded in tain.exe and unpacked alone, so anything they import
# must already be on every Windows machine. MinGW's runtime DLLs are not, and
# each of them has arrived here unasked at least once.
for dll in avutil-59.dll swresample-5.dll avcodec-61.dll; do
    if objdump -p "$staged/bin/$dll" | grep 'DLL Name' |
        grep -iE 'libwinpthread|libgcc|libstdc|libssp|libopus|zlib|libiconv'; then
        die "$dll imports a MinGW runtime DLL; nothing has been copied"
    fi
done

# ── Into the repository ─────────────────────────────────────────────────────
mkdir -p "$dest"
# Files, then whatever folders that leaves empty, rather than `rm -rf` on the
# folders: Windows will not remove a folder that is any process's current
# directory, an Explorer window or a terminal included, and that failure would
# come after the old files were already gone.
find "$dest" -mindepth 1 -type f ! -path "$dest/PROVENANCE.md" -delete
find "$dest" -mindepth 1 -depth -type d -empty -exec rmdir {} + 2>/dev/null || true
mkdir -p "$dest/bin" "$dest/include"
for dll in avutil-59.dll swresample-5.dll avcodec-61.dll; do
    cp "$staged/bin/$dll" "$dest/bin/"
done
for lib in libavcodec libavutil libswresample; do
    cp -r "$staged/include/$lib" "$dest/include/"
done
src=$work/ffmpeg-$FFMPEG_VERSION
cp "$src/LICENSE.md" "$src/COPYING.LGPLv3" "$src/COPYING.GPLv3" "$dest/"
cp "$work/opus-$OPUS_VERSION/COPYING" "$dest/COPYING.libopus"

# ── What goes in PROVENANCE.md ──────────────────────────────────────────────
echo
echo "FFmpeg   $FFMPEG_VERSION ($FFMPEG_SHA256)"
echo "libopus  $OPUS_VERSION ($OPUS_SHA256)"
echo "gcc      $(gcc -dumpversion) ($(gcc --version | head -n1))"
echo "binutils $(ld --version | head -n1)"
echo
echo "configure ${ffmpeg_configure[*]}"
echo
(cd "$dest/bin" && sha256sum -- *.dll && ls -l -- *.dll)
