# FFmpeg, as vendored

These DLLs are built from source for Tain by
[`scripts/build-ffmpeg.sh`](../../scripts/build-ffmpeg.sh). They are nobody's
published binaries, so this file is the record of what went into them.

FFmpeg is LGPL-3.0-or-later as configured here: `--enable-version3`, and no
`--enable-gpl`. See `LICENSE.md`, `COPYING.LGPLv3` and `COPYING.GPLv3` beside
this file. libopus, compiled into avcodec, is BSD-3-Clause: see
`COPYING.libopus`.

`tain-core/build.rs` compiles the shim in `tain-core/ffmpeg/` against
`include/`, generates import libraries from the DLLs' export tables, and embeds
the three DLLs in `tain.exe`. `tain_core::ffmpeg_runtime` unpacks them on first
use.

## Sources

| | |
| --- | --- |
| FFmpeg | 7.1.5: `https://ffmpeg.org/releases/ffmpeg-7.1.5.tar.xz` |
| Tag | `n7.1.5`, commit `3a0867c2bfda4a4d4309ca1a8cbdc6175e67f587` |
| SHA-256 | `de668509caf9e35e3cd162473441fdb29538c6d96ed080292b3cf9e6fc5d558f` |
| Signature | `ffmpeg-7.1.5.tar.xz.asc` verifies against the FFmpeg release signing key, `FCF9 86EA 15E6 E293 A564 4F10 B432 2F04 D676 58D8` |
| libopus | 1.6.1: `https://downloads.xiph.org/releases/opus/opus-1.6.1.tar.gz` |
| SHA-256 | `6ffcb593207be92584df15b32466ed64bbec99109f007c82205f0194572411a1`, as xiph publishes it in `SHA256SUMS.txt` |

## Toolchain

MSYS2 UCRT64, from the 2026-09-27 base archive, updated on 2026-10-19:

| Package | Version |
| --- | --- |
| `mingw-w64-ucrt-x86_64-gcc` | 16.2.0-6 |
| `mingw-w64-ucrt-x86_64-binutils` | 2.47-4 |
| `mingw-w64-ucrt-x86_64-crt`, `-headers` | 14.0.0.r495.g63ebe6cdb-1 |
| `mingw-w64-ucrt-x86_64-nasm` | 3.02-1 |
| `mingw-w64-ucrt-x86_64-pkgconf` | 3.0.9-1 |
| `make` | 4.4.1-3 |

## Configuration

libopus, static, with its defaults otherwise. Its neural extras (DRED, deep PLC,
OSCE) are off unless asked for, and are not:

    ./configure --disable-shared --enable-static --disable-doc --disable-extra-programs

FFmpeg, as the DLLs report it (`avcodec_configuration()`), one option to a line:

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
    --enable-decoder='h264,hevc,av1,opus'
    --enable-encoder=libopus
    --enable-hwaccel='h264_d3d11va,h264_d3d11va2,hevc_d3d11va,hevc_d3d11va2,av1_d3d11va,av1_d3d11va2'
    --extra-ldflags='-static-libgcc -no-pthread -Wl,--no-insert-timestamp'

`/opt/ffmpeg` is a placeholder: the script installs under a staging directory,
so no path from the machine that built them is in the DLLs.

## The result

| File | Bytes | SHA-256 |
| --- | --- | --- |
| `bin/avcodec-61.dll` | 3,581,952 | `95a4c4b91a60094db7c119e7cd03238fc3dffcb545a87235004b1a0e1ed3daf1` |
| `bin/avutil-59.dll` | 1,039,360 | `c8234fae52efc8a8d64a83249d73b62ef8e0c6cb9a277941c7d243a9f41319a1` |
| `bin/swresample-5.dll` | 120,320 | `871d6bfef4aef9fdc7fc9220639de297a4f6c3da0a948077c187ae893198157e` |

4.7 MB of DLLs, and 1 MB of headers for the three libraries in `include/`. The
general-purpose build this replaced was 149 MB, 66 MB of it avcodec.

Check one against it with:

    Get-FileHash .\bin\avcodec-61.dll -Algorithm SHA256

No PE timestamp is written: `--no-insert-timestamp` for the linker, and
`SOURCE_DATE_EPOCH=0` for the `strip` that runs when FFmpeg installs. Two
separate runs of the script on 2026-10-19 produced these exact bytes. A
different toolchain will not, since the compiler's choices are in the code.

## What is in them, and why

| Component | For |
| --- | --- |
| `h264`, `hevc`, `av1` decoders | Parsing the stream for hardware decode. The pictures are decoded by the GPU |
| `h264_d3d11va2`, `hevc_d3d11va2`, `av1_d3d11va2` hwaccels | The hardware decode itself. The `d3d11va2` ones because they output `AV_PIX_FMT_D3D11`, the one format `db_decode.c` accepts |
| `h264_d3d11va`, `hevc_d3d11va`, `av1_d3d11va` hwaccels | Unused. FFmpeg 7.1's Makefile compiles the file both kinds live in only for these, so without them the `d3d11va2` ones fail to link |
| `opus` decoder | The guest's sound. FFmpeg's own decoder, not libopus's |
| `libopus` encoder | The host's sound |
| swresample | Only because FFmpeg's opus decoder resamples through it |

`--disable-everything` removes every other component, and `--disable-autodetect`
every library the machine that built it happened to have. There are no parsers
or bitstream filters: the shim hands libavcodec whole access units, and none of
these decoders asks for a filter of its own.

Win32 threads rather than pthreads, libgcc linked in statically, and
`-no-pthread`, which stops MSYS2's gcc quietly linking winpthreads. The DLLs
need nothing beside them but each other: they import only Windows system
libraries and the Universal CRT, and the script refuses a build that imports
any of MinGW's runtime DLLs.

The cost of `-no-pthread` is that configure finds no `clock_gettime` or
`nanosleep`, so FFmpeg takes the fallbacks an MSVC build takes:
`av_gettime_relative` reads the wall clock (nothing Tain uses calls it), and
`av_usleep`, which the D3D11VA code calls to wait 2 ms when the GPU is busy,
sleeps through `usleep`.

## Replacing them

Run the script (see its header), then update this file from what it prints,
regenerate `licenses/ffmpeg-configuration.txt`, and follow
`licenses/README.md`. Moving past FFmpeg 7.1 changes the DLL names
(avcodec-62 and later) and is a change to the shim and both build scripts, not
only to this folder.
