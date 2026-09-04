# Virtual display driver

This folder is where Tain looks for a virtual display driver package. Put the
extracted contents of one here — the `.inf`, its `.cat`, and the driver binary
— and the **Virtual display** section of Settings will offer to install it.
Leave it empty and that section says so, and everything else works as usual.

Exactly one `.inf`, so that dropping a package in is the whole job. Tain reads
the class, the hardware id and the device name out of it rather than assuming
any of them, so a different package needs no code change.

## What must not be edited

Anything. A driver package's `.cat` catalog holds a signed hash of every file
in the package, the `.inf` included, so it installs byte for byte or not at
all. Renaming the device, tidying the `.inf`, re-zipping with different line
endings — each of those voids the signature, and Windows then refuses the
package or wants the machine in test-signing mode.

Which is also why the name Windows shows in its "do you trust this publisher"
dialog is the driver author's and not Tain's. That is honest: it is their
driver, signed by them.

## The package this targets

[Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver),
MIT licensed, by Mike The Tech and contributors. Its `.inf` declares:

    ClassGuid = {4D36E968-E325-11CE-BFC1-08002BE10318}
    Hardware id = Root\MttVDD
    DeviceName = "Virtual Display Driver"

Release **25.7.23**, the driver-only assets, as published by the GitHub API:

| asset | bytes | sha256 |
| --- | --- | --- |
| `VirtualDisplayDriver-x86.Driver.Only.zip` | 132118 | `e24210692b442b39af763536330ce78b423f19342b7a7792c26de3944e418b3a` |
| `VirtualDisplayDriver-ARM64.Driver.Only.zip` | 129547 | `dcd487161469c726bab60f174a7e7f70ee02afd437cad0d5c0e6755953de5a25` |

Check what you downloaded against those before putting it here:

    Get-FileHash .\VirtualDisplayDriver-x86.Driver.Only.zip -Algorithm SHA256

Recorded rather than fetched, and recorded per release rather than tracking
whatever is newest, because "the latest one" is not a thing that can be
verified in advance. The digests above come from GitHub's own release
metadata; they are worth checking against, not worth trusting alone.

## Two things to confirm before shipping this

- **`x86` in the asset name.** The other asset is `ARM64`, which suggests this
  one is the 64-bit Intel build under an old name rather than a 32-bit one.
  Read the `.inf`'s architecture-decorated sections to be sure; installing the
  wrong architecture fails in a way that reports success.
- **ARM64 signing.** The project's own tracker has had the ARM64 package
  refused for signing reasons where x64 was fine. Worth testing on an ARM
  machine before offering the button there.

## Licence

MIT. Keep `LICENSE` from the package here alongside it — that is the whole of
what redistribution asks.
