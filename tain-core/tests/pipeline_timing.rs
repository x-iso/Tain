//! Where the per-frame time actually goes.
//!
//! Not an assertion suite — a measurement. Run it when the stream feels slow:
//!
//! ```bash
//! cargo test --release -p tain-core --test pipeline_timing -- --ignored --nocapture
//! ```
//!
//! Build it in **release**. A debug build's numbers are dominated by unoptimised
//! pixel loops and will send you chasing the wrong stage.

use tain_core::codec_h264::{H264Decoder, H264Encoder};
use tain_core::screen_capture::primary_display_size;
use tain_core::{CaptureConfig, EncodeConfig, QualityScale, VideoCodec};
use std::time::{Duration, Instant};

/// Time `f` over several runs and report the median.
///
/// Median rather than mean: the first frame pays for encoder setup and the odd
/// frame collides with whatever else the machine is doing, and one outlier
/// should not decide the number you act on.
fn median_ms(runs: usize, mut f: impl FnMut()) -> f64 {
    let mut samples: Vec<Duration> = Vec::with_capacity(runs);
    for _ in 0..runs {
        let start = Instant::now();
        f();
        samples.push(start.elapsed());
    }
    samples.sort();
    samples[samples.len() / 2].as_secs_f64() * 1000.0
}

/// Turn on logging, so the encoder's own diagnostics reach `--nocapture`.
///
/// The measurements here are usually run *because* something is wrong, and the
/// encoder reporting which of its settings a driver refused is often the answer.
fn enable_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tain_core=debug".into()),
        )
        .try_init();
}

fn synthetic_desktop(width: u16, height: u16, shift: u8) -> Vec<u8> {
    // Gradients and blocks: compressible like a desktop, not like noise.
    let mut data = vec![0u8; width as usize * height as usize * 4];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let i = (y * width as usize + x) * 4;
            let block = ((x / 64) + (y / 64)) % 2 == 0;
            data[i] = if block { (x % 200) as u8 } else { 24 };
            data[i + 1] = if block { (y % 200) as u8 } else { 26 };
            data[i + 2] = if block { 200u8.wrapping_add(shift) } else { 30 };
            data[i + 3] = 255;
        }
    }
    data
}

/// A static desktop with one window-sized region moving across it.
///
/// This is what a desktop actually looks like to an encoder: most of the frame
/// identical to the last one, a bounded region changing. Rate-control behaviour
/// measured against full-frame noise says nothing about a real session.
fn desktop_with_moving_window(width: u16, height: u16, offset: usize) -> Vec<u8> {
    let mut data = synthetic_desktop(width, height, 0);
    let (w, h) = (width as usize, height as usize);
    let (bw, bh) = (w / 3, h / 3);
    let x0 = offset % w.saturating_sub(bw).max(1);
    let y0 = (offset / 3) % h.saturating_sub(bh).max(1);

    for y in y0..(y0 + bh).min(h) {
        for x in x0..(x0 + bw).min(w) {
            let i = (y * w + x) * 4;
            data[i] = ((x + offset) % 255) as u8;
            data[i + 1] = ((y + offset) % 255) as u8;
            data[i + 2] = 180;
        }
    }
    data
}

#[test]
#[ignore = "measurement, not an assertion — run explicitly"]
fn where_the_frame_time_goes() {
    let native = primary_display_size();
    println!("\nPrimary display: {}x{}", native.0, native.1);
    println!(
        "{:<26} {:>10} {:>10} {:>12}",
        "stage", "ms/frame", "max fps", "notes"
    );
    println!("{}", "-".repeat(62));

    for quality in QualityScale::ALL {
        let (w, h) = quality.apply(native);
        let frame = synthetic_desktop(w, h, 0);
        let moved = synthetic_desktop(w, h, 60);

        let mut encoder = H264Encoder::new(&EncodeConfig {
            codec: VideoCodec::H264,
            fps: 30,
            ..EncodeConfig::default()
        })
        .expect("encoder");

        // Warm up: the first call builds the encoder for this size.
        let (first, _) = encoder.encode(&frame, w, h).expect("encode");

        let mut flip = false;
        let encode_ms = median_ms(20, || {
            flip = !flip;
            let src = if flip { &moved } else { &frame };
            encoder.encode(src, w, h).expect("encode");
        });

        // A still screen is the cheap case and the one people quote; a changing
        // one is what the session actually costs.
        let still_ms = median_ms(10, || {
            encoder.encode(&frame, w, h).expect("encode");
        });

        let bytes_per_frame = w as usize * h as usize * 4;
        println!(
            "{:<26} {:>10.1} {:>10.1} {:>12}",
            format!("encode {} {}x{}", quality.label(), w, h),
            encode_ms,
            1000.0 / encode_ms.max(0.001),
            format!("{} KiB raw", bytes_per_frame / 1024),
        );
        println!(
            "{:<26} {:>10.1} {:>10.1} {:>12}",
            "  (static screen)",
            still_ms,
            1000.0 / still_ms.max(0.001),
            format!("key {} KiB", first.len() / 1024),
        );
    }

    // Decode, on the guest's UI-adjacent path.
    let (w, h) = QualityScale::Native.apply(native);
    let mut encoder = H264Encoder::new(&EncodeConfig::default()).expect("encoder");
    let frame = synthetic_desktop(w, h, 0);
    let (packet, _) = encoder.encode(&frame, w, h).expect("encode");
    let mut decoder = H264Decoder::new().expect("decoder");
    decoder.decode(&packet).expect("decode");

    let decode_ms = median_ms(10, || {
        let (packet, _) = encoder.encode(&frame, w, h).expect("encode");
        decoder.decode(&packet).expect("decode");
    });
    println!(
        "{:<26} {:>10.1} {:>10.1} {:>12}",
        format!("decode+encode {w}x{h}"),
        decode_ms,
        1000.0 / decode_ms.max(0.001),
        "guest side"
    );

    println!(
        "\nBudget at 30fps is 33.3 ms/frame; at 60fps, 16.7 ms.\n\
         Anything at or above the budget means the stream is capture-bound,\n\
         not network-bound — the bandwidth graph will look idle.\n"
    );
}

/// Software against hardware, on the same frames at the same sizes.
///
/// The number that matters is ms/frame at native resolution: it decides whether
/// the stream can hold 60 fps without the adaptive controller having to shed
/// pixels.
#[test]
#[ignore = "measurement, not an assertion — run explicitly"]
fn software_versus_hardware_encoding() {
    use tain_core::encoder::{create_encoder, hardware_encoder_name, EncoderBackend};

    let native = primary_display_size();
    println!("\nPrimary display: {}x{}", native.0, native.1);
    match hardware_encoder_name() {
        Some(name) => println!("Hardware encoder: {name}"),
        None => {
            println!("No hardware encoder on this machine — nothing to compare.");
            return;
        }
    }
    println!("{:<24} {:>10} {:>10} {:>12}", "backend / size", "ms/frame", "max fps", "KiB/frame");
    println!("{}", "-".repeat(60));

    for quality in QualityScale::ALL {
        let (w, h) = quality.apply(native);
        let frame = synthetic_desktop(w, h, 0);
        let moved = synthetic_desktop(w, h, 60);

        for backend in [EncoderBackend::Software, EncoderBackend::Hardware] {
            let config = EncodeConfig {
                codec: VideoCodec::H264,
                backend,
                fps: 60,
                ..EncodeConfig::default()
            };
            let Ok(mut encoder) = create_encoder(config) else {
                println!("{:<24} {:>10}", format!("{backend:?} {w}x{h}"), "unavailable");
                continue;
            };

            // Warm up: builds the encoder, and lets a hardware transform fill
            // its pipeline so the timed frames are steady-state.
            for i in 0..8 {
                let src = if i % 2 == 0 { &frame } else { &moved };
                encoder.encode_frame(src, w, h, tain_core::PixelFormat::Bgra).expect("encode");
            }

            let mut flip = false;
            let mut bytes = 0usize;
            let ms = median_ms(20, || {
                flip = !flip;
                let src = if flip { &moved } else { &frame };
                bytes = encoder.encode_frame(src, w, h, tain_core::PixelFormat::Bgra).expect("encode").data.len();
            });

            println!(
                "{:<24} {:>10.2} {:>10.1} {:>12}",
                format!("{backend:?} {w}x{h}"),
                ms,
                1000.0 / ms.max(0.001),
                bytes / 1024,
            );
        }
    }
    println!("\n16.7 ms is the 60fps budget, 8.3 ms the 120fps one.\n");
}

/// The real pipeline, end to end, on the real desktop.
///
/// The synthetic benchmarks above measure encode in isolation and say nothing
/// about what a session actually costs. This runs the same capture backend and
/// encoder a session uses, and reports **max as well as median** — a freeze is a
/// tail-latency event, and a median hides it completely.
/// Does asking for a lower bitrate actually produce one?
///
/// This is load-bearing. When the link cannot carry the stream the adaptive
/// controller's only real lever is bitrate, and if the encoder ignores it the
/// controller backs off into a wall: the queue stays full, the capture thread
/// stays blocked, and the picture stops moving with no way out.
#[test]
#[ignore = "measurement, not an assertion — run explicitly"]
fn a_bitrate_limit_is_actually_honoured() {
    use tain_core::encoder::{create_encoder, EncoderBackend};
    enable_logging();

    let (w, h) = QualityScale::Native.apply(primary_display_size());
    println!("\nAt {w}x{h}, 60fps:");
    println!("{:<12} {:>14} {:>14} {:>10}", "backend", "requested", "measured", "ratio");
    println!("{}", "-".repeat(54));

    for backend in [EncoderBackend::Hardware, EncoderBackend::Software] {
        for requested in [2_000u32, 8_000, 25_000] {
            let config = EncodeConfig {
                codec: VideoCodec::H264,
                backend,
                fps: 60,
                target_bitrate_kbps: requested,
                ..EncodeConfig::default()
            };
            let Ok(mut encoder) = create_encoder(config) else { continue };

            // Three seconds of *desktop-like* motion: a window moving over a
            // static background. A full-screen flip every frame — which an
            // earlier version of this used — is worse than anything a real
            // desktop does, and made a working rate controller look broken.
            let frames: Vec<Vec<u8>> =
                (0..30).map(|i| desktop_with_moving_window(w, h, i * 8)).collect();
            for i in 0..15 {
                encoder.encode_frame(&frames[i % frames.len()], w, h, tain_core::PixelFormat::Bgra).expect("encode");
            }

            let count = 180;
            let mut total = 0usize;
            for i in 0..count {
                let out = encoder
                    .encode_frame(&frames[i % frames.len()], w, h, tain_core::PixelFormat::Bgra)
                    .expect("encode");
                total += out.data.len();
            }

            // 180 frames at 60fps is three seconds.
            let measured = total as f64 * 8.0 / 1000.0 / 3.0;
            println!(
                "{:<12} {:>11} kbps {:>11.0} kbps {:>9.2}x",
                format!("{backend:?}"),
                requested,
                measured,
                measured / requested as f64,
            );
        }
    }
    println!("\nA ratio far above 1 means the limit is being ignored.\n");
}

#[test]
#[ignore = "requires a real desktop session"]
fn where_a_real_session_spends_its_time() {
    use tain_core::encoder::{create_encoder, EncoderBackend};

    let config = CaptureConfig {
        display: None,
        fps: 60,
        quality: QualityScale::Native,
        show_cursor: true,
        native_override: None,
    };
    let mut capturer = tain_core::screen_capture::create_capturer(config).expect("capturer");
    let (w, h) = capturer.frame_size();
    println!("\nCapture backend: {w}x{h}, real={}", capturer.is_real());

    let mut encoder = create_encoder(EncodeConfig {
        codec: VideoCodec::H264,
        backend: EncoderBackend::Auto,
        fps: 60,
        ..EncodeConfig::default()
    })
    .expect("encoder");

    // Exactly what a session does before it starts streaming.
    let format = tain_core::screen_capture::negotiate_format(
        &mut *capturer,
        encoder.preferred_format(),
    );
    println!("Pixel format: {format:?} (encoder wants {:?})", encoder.preferred_format());

    let mut capture_ms = Vec::new();
    let mut waited_ms = Vec::new();
    let mut encode_ms = Vec::new();
    let mut bytes = Vec::new();
    let mut empty = 0;

    // Warm up: duplication and the encoder both settle over the first frames.
    for _ in 0..10 {
        if let Ok(Some(f)) = capturer.capture_next() {
            let _ = encoder.encode_frame(&f.data, f.width, f.height, f.format);
        }
    }

    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(6) {
        let t0 = Instant::now();
        let Ok(Some(frame)) = capturer.capture_next() else { continue };
        let t1 = Instant::now();
        let encoded = encoder
            .encode_frame(&frame.data, frame.width, frame.height, frame.format)
            .expect("encode");
        let t2 = Instant::now();

        // The backend's own figure, not wall time. `capture_next` blocks until
        // the desktop changes, and charging that wait to capture is how an idle
        // screen ends up looking like a slow one — the same mistake the
        // bottleneck detector was fixed for.
        capture_ms.push(frame.cost.as_secs_f64() * 1000.0);
        waited_ms.push((t1 - t0).as_secs_f64() * 1000.0);
        encode_ms.push((t2 - t1).as_secs_f64() * 1000.0);
        if encoded.data.is_empty() {
            empty += 1;
        } else {
            bytes.push(encoded.data.len());
        }
    }

    let stat = |name: &str, mut v: Vec<f64>| {
        if v.is_empty() {
            println!("{name:<12} no samples");
            return;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "{name:<12} median {:>7.2} ms   p95 {:>7.2} ms   max {:>7.2} ms",
            v[v.len() / 2],
            v[v.len() * 95 / 100],
            v[v.len() - 1],
        );
    };

    let frames = capture_ms.len();
    println!("\n{frames} frames in 6s = {:.1} fps achieved", frames as f64 / 6.0);
    stat("capture", capture_ms);
    stat("  +waiting", waited_ms);
    stat("encode", encode_ms);

    let total: usize = bytes.iter().sum();
    println!(
        "bitrate      {:.1} Mbit/s   ({} empty frames, largest {} KiB)",
        total as f64 * 8.0 / 6.0 / 1_000_000.0,
        empty,
        bytes.iter().max().copied().unwrap_or(0) / 1024,
    );
    println!("\nBudget is 16.7 ms/frame at 60fps.\n");
}

#[test]
#[ignore = "requires a real desktop session"]
fn how_long_a_real_capture_takes() {
    let config = CaptureConfig {
        display: None,
        fps: 30,
        quality: QualityScale::Native,
        show_cursor: true,
        native_override: None,
    };
    let mut capturer = tain_core::screen_capture::create_capturer(config).expect("capturer");
    let (w, h) = capturer.frame_size();
    println!("\nCapture backend reports {w}x{h}, real={}", capturer.is_real());

    // Discard the first few: duplication warms up.
    for _ in 0..5 {
        let _ = capturer.capture_next();
    }

    let capture_ms = median_ms(30, || {
        let _ = capturer.capture_next().expect("capture");
    });
    println!("capture: {capture_ms:.1} ms/frame ({:.1} fps ceiling)\n", 1000.0 / capture_ms.max(0.001));
}

/// What a GPU capture path would actually remove.
///
/// Two costs stand between the desktop and the encoder, and both are pure
/// plumbing: reading 4 bytes per pixel back across PCIe, and turning those bytes
/// into NV12 on the CPU. Neither compresses anything. This measures them against
/// the encode they surround, so the case for moving them onto the GPU is a
/// number rather than an assumption.
#[test]
#[ignore = "measurement, not an assertion — run explicitly"]
#[cfg(target_os = "windows")]
fn what_the_colour_conversion_costs() {
    use tain_core::codec_mf;

    let native = primary_display_size();
    let (w, h) = QualityScale::Native.apply(native);
    let frame = synthetic_desktop(w, h, 0);
    let mut nv12 = vec![0u8; w as usize * h as usize * 3 / 2];

    println!("\nAt {w}x{h}:");

    let convert_ms = median_ms(30, || {
        codec_mf::bgra_to_nv12_into(&frame, w, h, &mut nv12);
    });
    println!("  BGRA -> NV12 on the CPU     {convert_ms:>7.2} ms");

    // The readback is not directly measurable without a capture, so quote what
    // the capture test reports and how much of it is bytes moved.
    let bgra_mb = (w as f64 * h as f64 * 4.0) / (1024.0 * 1024.0);
    let nv12_mb = bgra_mb * 3.0 / 8.0;
    println!("  frame as BGRA               {bgra_mb:>7.2} MB  (what is read back today)");
    println!("  frame as NV12               {nv12_mb:>7.2} MB  (what a GPU convert would read)");

    let mut encoder = tain_core::encoder::create_encoder(EncodeConfig {
        codec: VideoCodec::H264,
        backend: tain_core::EncoderBackend::Hardware,
        fps: 60,
        ..EncodeConfig::default()
    })
    .expect("encoder");
    let moved = desktop_with_moving_window(w, h, 0);
    encoder.encode_frame(&frame, w, h, tain_core::PixelFormat::Bgra).expect("encode");
    let mut n = 0usize;
    let encode_ms = median_ms(20, || {
        n += 1;
        let src = if n.is_multiple_of(2) { &frame } else { &moved };
        encoder.encode_frame(src, w, h, tain_core::PixelFormat::Bgra).expect("encode");
    });
    println!("  encode_frame (incl. convert){encode_ms:>7.2} ms");
    println!(
        "  => conversion is {:.0}% of encode; the rest is the GPU actually compressing.\n",
        100.0 * convert_ms / encode_ms.max(0.001)
    );
}

/// The two ways to get a desktop frame off the GPU, measured against each other.
///
/// Reading BGRA back and converting on the CPU moves 15.6 MB across PCIe and
/// then spends milliseconds on scalar arithmetic. Converting first moves 5.9 MB
/// and spends nothing — *if* the blit itself is free, which is the part worth
/// measuring rather than assuming. Same texture, same device, back to back.
#[test]
#[ignore = "requires a D3D11 device with video support"]
#[cfg(target_os = "windows")]
fn gpu_conversion_against_cpu_readback() {
    use tain_core::capture_nv12::Nv12Converter;
    use windows::core::Interface;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::*;

    let (w, h) = QualityScale::Native.apply(primary_display_size());
    println!("\nAt {w}x{h}:");

    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .expect("D3D11 device with video support");
    let (device, context) = (device.unwrap(), context.unwrap());
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        unsafe { let _ = mt.SetMultithreadProtected(true); };
    }

    // Stands in for the duplication surface: same format, same usage.
    let source = {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w as u32,
            Height: h as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET).0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let pixels = synthetic_desktop(w, h, 0);
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr() as *const _,
            SysMemPitch: w as u32 * 4,
            SysMemSlicePitch: 0,
        };
        let mut t = None;
        unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut t)) }.expect("source");
        t.unwrap()
    };

    // Path A: copy to a BGRA staging texture, read it back, convert on the CPU.
    let bgra_staging = {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w as u32,
            Height: h as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut t = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut t)) }.expect("staging");
        t.unwrap()
    };

    let mut bgra = vec![0u8; w as usize * h as usize * 4];
    let readback_ms = median_ms(20, || unsafe {
        context.CopyResource(&bgra_staging, &source);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&bgra_staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).unwrap();
        let pitch = mapped.RowPitch as usize;
        let row = w as usize * 4;
        for y in 0..h as usize {
            std::ptr::copy_nonoverlapping(
                (mapped.pData as *const u8).add(y * pitch),
                bgra[y * row..].as_mut_ptr(),
                row,
            );
        }
        context.Unmap(&bgra_staging, 0);
    });

    let mut nv12 = vec![0u8; w as usize * h as usize * 3 / 2];
    let convert_ms = median_ms(20, || {
        tain_core::codec_mf::bgra_to_nv12_into(&bgra, w, h, &mut nv12);
    });

    // Path B: convert on the GPU, read back the smaller result.
    let mut converter = Nv12Converter::new(&device, &context, w, h).expect("converter");
    converter.convert(&source).expect("warm up");
    let gpu_ms = median_ms(20, || {
        converter.convert(&source).expect("convert");
    });

    println!("  BGRA readback              {readback_ms:>7.2} ms");
    println!("  CPU convert                {convert_ms:>7.2} ms");
    println!("  ---------------------------------");
    println!("  today's path (both)        {:>7.2} ms", readback_ms + convert_ms);
    println!("  GPU convert + NV12 readback{gpu_ms:>7.2} ms");
    let delta = readback_ms + convert_ms - gpu_ms;
    println!(
        "  => {} by {:.2} ms/frame\n",
        if delta > 0.0 { "faster" } else { "SLOWER" },
        delta.abs()
    );
}

/// The same real desktop, captured and encoded both ways.
///
/// The controlled microbenchmark says the GPU path wins by 2.4 ms a frame. This
/// checks that against a live desktop with the hardware encoder running at the
/// same time, because that is the case that decides anything: the video
/// processor and the encoder share one GPU, and a win that evaporates under
/// contention is not a win. Two passes, one after the other, so only one
/// duplication exists at a time.
#[test]
#[ignore = "requires a real desktop session"]
fn a_real_session_both_ways() {
    use tain_core::encoder::{create_encoder, EncoderBackend};
    use tain_core::screen_capture::{negotiate_format, PixelFormat};

    let pass = |want: PixelFormat| {
        let config = CaptureConfig {
        display: None,
            fps: 120,
            quality: QualityScale::Native,
            show_cursor: true,
            native_override: None,
        };
        let mut capturer =
            tain_core::screen_capture::create_capturer(config).expect("capturer");
        let mut encoder = create_encoder(EncodeConfig {
            codec: VideoCodec::H264,
            backend: EncoderBackend::Auto,
            fps: 120,
            ..EncodeConfig::default()
        })
        .expect("encoder");
        let format = negotiate_format(&mut *capturer, want);

        for _ in 0..20 {
            if let Ok(Some(f)) = capturer.capture_next() {
                let _ = encoder.encode_frame(&f.data, f.width, f.height, f.format);
            }
        }

        let (mut cap, mut enc) = (Vec::new(), Vec::new());
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(5) {
            let t0 = Instant::now();
            let Ok(Some(frame)) = capturer.capture_next() else { continue };
            let t1 = Instant::now();
            encoder
                .encode_frame(&frame.data, frame.width, frame.height, frame.format)
                .expect("encode");
            let t2 = Instant::now();
            // The backend's own figure: `capture_next` blocks waiting for the
            // desktop to change, and that wait is not a cost.
            cap.push(frame.cost.as_secs_f64() * 1000.0);
            enc.push((t2 - t1).as_secs_f64() * 1000.0);
            let _ = t0;
        }
        cap.sort_by(|a, b| a.partial_cmp(b).unwrap());
        enc.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (format, cap, enc)
    };

    println!();
    let mut results = Vec::new();
    for want in [PixelFormat::Bgra, PixelFormat::Nv12] {
        let (format, cap, enc) = pass(want);
        if cap.is_empty() {
            println!("{format:?}: no frames — is the desktop completely static?");
            continue;
        }
        let med = |v: &Vec<f64>| v[v.len() / 2];
        let p95 = |v: &Vec<f64>| v[v.len() * 95 / 100];
        println!(
            "{:<6} {:>4} frames   capture {:>6.2} ms (p95 {:>6.2})   encode {:>6.2} ms (p95 {:>6.2})   total {:>6.2} ms",
            format!("{format:?}"),
            cap.len(),
            med(&cap),
            p95(&cap),
            med(&enc),
            p95(&enc),
            med(&cap) + med(&enc),
        );
        results.push((format, med(&cap) + med(&enc)));
    }

    if let [(_, bgra), (_, nv12)] = results[..] {
        println!(
            "\n  GPU conversion is {:.2} ms/frame {} — {:.0} fps against {:.0} fps.\n",
            (bgra - nv12).abs(),
            if nv12 < bgra { "faster" } else { "SLOWER" },
            1000.0 / nv12,
            1000.0 / bgra,
        );
    }
}

/// What it costs to ask "is there a GPU encoder?"
///
/// The answer looks like a lookup and is not: it enumerates Media Foundation
/// transforms and then **instantiates** one, which loads the vendor driver and
/// allocates a real encode session. Anything calling it per frame will stall.
#[test]
#[ignore = "measurement, not an assertion — run explicitly"]
#[cfg(target_os = "windows")]
fn what_asking_for_the_encoder_name_costs() {
    use tain_core::encoder::hardware_encoder_name;

    // First call also starts Media Foundation, so it is not representative.
    let first = std::time::Instant::now();
    let name = hardware_encoder_name();
    let first = first.elapsed();
    println!("\n  first call (starts MF)     {:>8.2} ms  -> {name:?}", first.as_secs_f64() * 1000.0);

    let steady = median_ms(10, || {
        std::hint::black_box(hardware_encoder_name());
    });
    println!("  steady state               {steady:>8.2} ms");
    println!(
        "  at 60 fps that is {:.0}% of the frame budget; the settings panel called it twice a frame.\n",
        steady / 16.7 * 100.0
    );
}

/// Everything the settings panel asks for while it is on screen.
///
/// A UI redraws its whole contents every frame, so anything it calls is called
/// at frame rate. Two of these look like field accesses and are not.
#[test]
#[ignore = "measurement, not an assertion — run explicitly"]
fn what_the_settings_panel_asks_for_per_frame() {
    use tain_core::screen_capture::primary_display_size;

    println!();
    let probe_ms = median_ms(20, || {
        std::hint::black_box(tain_core::encoder::hardware_encoder_name());
    });
    println!("  hardware_encoder_name      {probe_ms:>8.3} ms");

    let display_ms = median_ms(20, || {
        std::hint::black_box(primary_display_size());
    });
    println!("  primary_display_size       {display_ms:>8.3} ms");

    let avail_ms = median_ms(20, || {
        for c in VideoCodec::ALL {
            std::hint::black_box(c.is_available());
        }
    });
    println!("  every codec's availability {avail_ms:>8.3} ms");

    let total = probe_ms + display_ms + avail_ms;
    println!(
        "  -> {total:.3} ms per frame, {:.1}% of a 16.7 ms budget\n",
        total / 16.7 * 100.0
    );
}


/// What `thread::sleep` actually does when asked for a frame interval.
///
/// The capture thread paces itself with `thread::sleep(next_frame_at - now)`.
/// On Windows the system timer runs at 15.6 ms unless a process raises it, and
/// a sleep is rounded *up* to the next tick — so asking for 16.7 ms can cost 31.
/// That would cap capture at half the requested rate no matter how fast the
/// hardware is.
#[test]
#[ignore = "measurement, not an assertion — run explicitly"]
fn what_a_frame_interval_sleep_really_costs() {
    println!();
    for fps in [30u32, 60, 120, 160] {
        let interval = Duration::from_micros(1_000_000 / fps as u64);
        let mut slept = Vec::new();
        for _ in 0..30 {
            let t0 = Instant::now();
            std::thread::sleep(interval);
            slept.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        slept.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = slept[slept.len() / 2];
        println!(
            "  {fps:>3} fps: asked {:>6.2} ms, got {:>6.2} ms  -> {:>5.1} fps achievable",
            interval.as_secs_f64() * 1000.0,
            median,
            1000.0 / median,
        );
    }
    println!();
}

/// What the guest still does on the CPU for every decoded frame.
#[test]
#[ignore = "measurement, not an assertion — run explicitly"]
#[cfg(target_os = "windows")]
fn what_the_guest_pays_after_the_gpu_decode() {
    use tain_core::codec_mf_decode::nv12_to_rgba;

    let (w, h) = (2038u16, 1274u16);
    let nv12 = vec![128u8; w as usize * h as usize * 3 / 2];
    let mut rgba = vec![0u8; w as usize * h as usize * 4];

    let ms = median_ms(30, || nv12_to_rgba(&nv12, w, h, &mut rgba));
    println!("\n  NV12 -> RGBA on the CPU at {w}x{h}: {ms:.2} ms/frame");
    println!("  ceiling from this alone: {:.0} fps", 1000.0 / ms.max(0.001));
    println!("  budget at 60 fps is 16.7 ms; at 160 fps, 6.25 ms.\n");
}
