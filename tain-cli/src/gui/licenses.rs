//! The licence texts behind Settings → About → "Licences…".
//!
//! Every entry embeds its licence **verbatim** with `include_str!` — never
//! retyped — so a notice here cannot drift from the real text, and a file that
//! goes missing is a build error rather than a notice that silently stopped
//! shipping:
//!
//! * Tain's own licence is the `LICENSE` at the root of the repository;
//! * the virtual display driver, FFmpeg and libopus point at the licences
//!   vendored beside them under `third_party/`;
//! * everything else points at `licenses/`, which holds copies taken from the
//!   published crate source or the upstream project, at the version built
//!   against. `licenses/README.md` says where each one came from.
//!
//! Scope is what Tain **redistributes**: code linked into `tain.exe`, the FFmpeg
//! libraries and the driver package embedded in it, and the icons and fonts it
//! draws with. Build tools are not redistributed and are not listed.
//!
//! # What is translated
//!
//! The one-line description of each entry is interface text, so it is a
//! catalogue key like everything else on screen. The rest is attribution — a
//! project's name, its SPDX expression, a note on provenance, and the licence
//! itself — and stays as its authors wrote it, because a licence is only
//! authoritative in the language it was written in and a translated copyright
//! line is a different claim.

/// Tain's own copyright line, shown in Settings and at the top of the list.
pub const COPYRIGHT: &str = "Copyright © 2026 x.iso";

/// One entry in the licence viewer.
pub struct License {
    /// Display name in the left-hand list.
    pub name: &'static str,
    /// Catalogue key for one line on what it is and why its bytes are in the
    /// build.
    pub what: &'static str,
    /// SPDX expression as the project declares it. Where it offers a choice
    /// ("MIT OR Apache-2.0") the text below is the option Tain takes, and this
    /// still records the full choice on offer.
    pub spdx: &'static str,
    /// Upstream home page.
    pub url: &'static str,
    /// Provenance or attribution the bare licence text does not carry.
    pub note: Option<&'static str>,
    /// The licence, verbatim.
    pub text: &'static str,
}

/// Tain first, then what it could least do without.
pub const LICENSES: &[License] = &[
    License {
        name: "Tain",
        what: "licences.tain",
        spdx: "GPL-3.0-or-later",
        url: "https://github.com/x-iso/Tain",
        note: Some(
            "Copyright © 2026 x.iso.\n\
             This program is free software: you can redistribute it and/or modify it \
             under the terms of the GNU General Public License as published by the Free \
             Software Foundation, either version 3 of the License, or (at your option) \
             any later version. It is distributed in the hope that it will be useful, \
             but WITHOUT ANY WARRANTY; without even the implied warranty of \
             MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.",
        ),
        text: include_str!("../../../LICENSE"),
    },
    License {
        name: "FFmpeg",
        what: "licences.ffmpeg",
        spdx: "LGPL-3.0-or-later",
        url: "https://ffmpeg.org",
        note: Some(
            "avcodec-61, avutil-59 and swresample-5 from FFmpeg 7.1.5, built for Tain \
             with only what it calls: the H.264, HEVC and AV1 decoders with D3D11VA, \
             the Opus decoder, and the libopus encoder. Source: \
             https://ffmpeg.org/releases/ffmpeg-7.1.5.tar.xz, which is tag n7.1.5 \
             (commit 3a0867c2bf) of https://git.ffmpeg.org/ffmpeg.git, built by \
             scripts/build-ffmpeg.sh in Tain's repository. \
             third_party/ffmpeg/PROVENANCE.md there records the toolchain and checksums.\n\
             Libraries of the same names placed beside tain.exe are loaded instead of \
             the embedded copies, so a modified FFmpeg can be substituted.\n\
             Below: the LGPL, the GPL it builds on, and the configure line these \
             DLLs were built with.",
        ),
        text: concat!(
            include_str!("../../../third_party/ffmpeg/COPYING.LGPLv3"),
            "\n\n",
            include_str!("../../../LICENSE"),
            "\n\n",
            include_str!("../../../licenses/ffmpeg-configuration.txt"),
        ),
    },
    License {
        name: "Opus",
        what: "licences.opus",
        spdx: "BSD-3-Clause",
        url: "https://opus-codec.org",
        note: Some("libopus 1.6.1, compiled into the FFmpeg libraries above."),
        text: include_str!("../../../third_party/ffmpeg/COPYING.libopus"),
    },
    License {
        name: "OpenH264",
        what: "licences.openh264",
        spdx: "BSD-2-Clause",
        url: "https://github.com/cisco/openh264",
        note: Some(
            "Built from the source carried by openh264-sys2 0.9.7. The Rust bindings, \
             openh264 and openh264-sys2 by Ralf Biedert, are BSD-2-Clause too.\n\
             H.264 is covered by patents. Cisco's royalty coverage applies only to \
             binaries Cisco distributes, and this is not one of them.",
        ),
        text: include_str!("../../../licenses/openh264.txt"),
    },
    License {
        name: "Virtual Display Driver",
        what: "licences.vdd",
        spdx: "MIT",
        url: "https://github.com/VirtualDrivers/Virtual-Display-Driver",
        note: Some("Release 25.7.23, unmodified — see third_party/VirtualDisplayDriver/PROVENANCE.md."),
        text: include_str!("../../../third_party/VirtualDisplayDriver/LICENSE"),
    },
    License {
        name: "Hugeicons",
        what: "licences.hugeicons",
        spdx: "MIT",
        url: "https://hugeicons.com",
        note: Some("From the free set, @hugeicons/core-free-icons."),
        text: include_str!("../../../licenses/hugeicons.txt"),
    },
    License {
        name: "iroh",
        what: "licences.iroh",
        spdx: "MIT OR Apache-2.0",
        url: "https://github.com/n0-computer/iroh",
        note: Some(
            "Covers iroh, iroh-relay, iroh-base and the other crates by n0 in the build, \
             including the mDNS and mainline DHT lookups. Parts of iroh are derived from \
             Tailscale; that notice is the next entry.",
        ),
        text: include_str!("../../../licenses/n0.txt"),
    },
    License {
        name: "Tailscale (in iroh)",
        what: "licences.tailscale",
        spdx: "BSD-3-Clause",
        url: "https://github.com/tailscale/tailscale",
        note: Some("As shipped by iroh 1.0.3 and iroh-relay 1.0.3, in that order."),
        text: concat!(
            include_str!("../../../licenses/iroh-tailscale.txt"),
            "\n\n",
            include_str!("../../../licenses/iroh-relay-tailscale.txt"),
        ),
    },
    License {
        name: "rustls and webpki",
        what: "licences.rustls",
        spdx: "Apache-2.0 OR ISC OR MIT",
        url: "https://github.com/rustls/rustls",
        note: Some("rustls 0.23 under MIT, then rustls-webpki, which is ISC."),
        text: concat!(
            include_str!("../../../licenses/rustls.txt"),
            "\n\n",
            include_str!("../../../licenses/rustls-webpki.txt"),
        ),
    },
    License {
        name: "ring",
        what: "licences.ring",
        spdx: "Apache-2.0 AND ISC",
        url: "https://github.com/briansmith/ring",
        note: Some("ring 0.17.14: its LICENSE, then LICENSE-BoringSSL and LICENSE-other-bits."),
        text: concat!(
            include_str!("../../../licenses/ring.txt"),
            "\n\n",
            include_str!("../../../licenses/ring-boringssl.txt"),
            "\n\n",
            include_str!("../../../licenses/ring-other-bits.txt"),
        ),
    },
    License {
        name: "dalek cryptography",
        what: "licences.dalek",
        spdx: "BSD-3-Clause",
        url: "https://github.com/dalek-cryptography/curve25519-dalek",
        note: Some("curve25519-dalek 5.0.0 and ed25519-dalek 3.0.0."),
        text: include_str!("../../../licenses/dalek.txt"),
    },
    License {
        name: "webpki-roots",
        what: "licences.webpki_roots",
        spdx: "CDLA-Permissive-2.0",
        url: "https://github.com/rustls/webpki-roots",
        note: None,
        text: include_str!("../../../licenses/webpki-roots.txt"),
    },
    License {
        name: "tokio",
        what: "licences.tokio",
        spdx: "MIT",
        url: "https://tokio.rs",
        note: None,
        text: include_str!("../../../licenses/tokio.txt"),
    },
    License {
        name: "egui / eframe",
        what: "licences.egui",
        spdx: "MIT OR Apache-2.0",
        url: "https://github.com/emilk/egui",
        note: Some(
            "egui, eframe and egui_extras 0.29.1. Their published crates carry no \
             licence file; this is LICENSE-MIT from the egui repository.",
        ),
        text: include_str!("../../../licenses/egui.txt"),
    },
    License {
        name: "Fonts",
        what: "licences.fonts",
        spdx: "UFL-1.0 AND MIT AND OFL-1.1",
        url: "https://github.com/emilk/egui/tree/master/crates/epaint_default_fonts",
        note: Some(
            "Shipped by epaint_default_fonts 0.29.1: Ubuntu Light (Ubuntu Font Licence), \
             Hack (MIT, with Bitstream Vera and DejaVu terms), Noto Emoji (SIL Open Font \
             Licence) and emoji-icon-font (MIT), in that order.",
        ),
        text: concat!(
            include_str!("../../../licenses/font-ubuntu.txt"),
            "\n\n",
            include_str!("../../../licenses/font-hack.txt"),
            "\n\n",
            include_str!("../../../licenses/font-noto-emoji.txt"),
            "\n\n",
            include_str!("../../../licenses/font-emoji-icon.txt"),
        ),
    },
    License {
        name: "resvg, usvg and attohttpc",
        what: "licences.mpl",
        spdx: "MPL-2.0",
        url: "https://github.com/RazrFalcon/resvg",
        note: Some(
            "resvg and usvg 0.37.0, and attohttpc 0.30.1. Their source is available \
             unmodified from crates.io and from https://github.com/RazrFalcon/resvg and \
             https://github.com/sbstp/attohttpc.",
        ),
        text: include_str!("../../../licenses/MPL-2.0.txt"),
    },
    License {
        name: "tiny-skia",
        what: "licences.tiny_skia",
        spdx: "BSD-3-Clause",
        url: "https://github.com/RazrFalcon/tiny-skia",
        note: None,
        text: include_str!("../../../licenses/tiny-skia.txt"),
    },
    License {
        name: "serde",
        what: "licences.serde",
        spdx: "MIT OR Apache-2.0",
        url: "https://github.com/serde-rs/serde",
        note: None,
        text: include_str!("../../../licenses/serde.txt"),
    },
    License {
        name: "bincode",
        what: "licences.bincode",
        spdx: "MIT",
        url: "https://crates.io/crates/bincode/1.3.3",
        note: Some("Version 1.3.3."),
        text: include_str!("../../../licenses/bincode.txt"),
    },
    License {
        name: "Zstandard",
        what: "licences.zstd",
        spdx: "BSD-3-Clause",
        url: "https://github.com/facebook/zstd",
        note: Some(
            "libzstd 1.5.7, compiled from source by zstd-sys, under the BSD half of its \
             BSD-or-GPLv2 choice. Then the Rust bindings, zstd 0.13.3, which are MIT.",
        ),
        text: concat!(
            include_str!("../../../licenses/zstd.txt"),
            "\n\n",
            include_str!("../../../licenses/zstd-rs.txt"),
        ),
    },
    License {
        name: "windows-rs",
        what: "licences.windows",
        spdx: "MIT OR Apache-2.0",
        url: "https://github.com/microsoft/windows-rs",
        note: None,
        text: include_str!("../../../licenses/windows-rs.txt"),
    },
    License {
        name: "keyring",
        what: "licences.keyring",
        spdx: "MIT OR Apache-2.0",
        url: "https://github.com/open-source-cooperative/keyring-rs",
        note: None,
        text: include_str!("../../../licenses/keyring.txt"),
    },
    License {
        name: "arboard",
        what: "licences.arboard",
        spdx: "MIT OR Apache-2.0",
        url: "https://github.com/1Password/arboard",
        note: None,
        text: include_str!("../../../licenses/arboard.txt"),
    },
    License {
        name: "ICU4X",
        what: "licences.icu",
        spdx: "Unicode-3.0",
        url: "https://github.com/unicode-org/icu4x",
        note: Some("The icu_* crates and their zerovec family, version 2.2."),
        text: include_str!("../../../licenses/unicode.txt"),
    },
    License {
        name: "All Rust crates",
        what: "licences.all_crates",
        spdx: "various",
        url: "https://crates.io",
        note: Some("Generated from Cargo.lock by scripts/list-crate-licences.ps1."),
        text: include_str!("../../../licenses/rust-crates.txt"),
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Each description is looked up by a key held in data, which the
    /// catalogue's own source scan cannot see — so it is checked here.
    #[test]
    fn every_description_is_in_the_english_catalogue() {
        let english = tain_core::i18n::languages()
            .iter()
            .find(|l| l.tag == "en")
            .expect("the English catalogue is compiled in");
        for entry in LICENSES {
            assert!(
                english.get(entry.what).is_some(),
                "{} names {}, which the English catalogue does not have",
                entry.name,
                entry.what
            );
        }
    }

    #[test]
    fn tain_comes_first_under_its_own_licence() {
        let first = &LICENSES[0];
        assert_eq!(first.name, "Tain");
        assert!(first.text.contains("GNU GENERAL PUBLIC LICENSE"));
        assert!(first.note.is_some_and(|n| n.starts_with(COPYRIGHT)));
    }

    #[test]
    fn names_are_distinct_and_nothing_is_empty() {
        let mut names = std::collections::BTreeSet::new();
        for entry in LICENSES {
            assert!(names.insert(entry.name), "{} is listed twice", entry.name);
            assert!(!entry.text.trim().is_empty(), "{} has no licence text", entry.name);
            assert!(entry.url.starts_with("https://"), "{} has no link", entry.name);
        }
    }
}
