# Virtual Display Driver, as vendored

These files are redistributed unmodified from
[VirtualDrivers/Virtual-Display-Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver),
MIT licensed — see `LICENSE` beside this file, which is the whole of what that
asks. `build.rs` compiles them into `tain.exe`; the **Virtual display** section
of Settings installs them.

| | |
| --- | --- |
| Release | `25.7.23` |
| Asset | `VirtualDisplayDriver-x86.Driver.Only.zip` |
| Size | 132,118 bytes |
| SHA-256 | `e24210692b442b39af763536330ce78b423f19342b7a7792c26de3944e418b3a` |

The digest is GitHub's own, published in the release metadata. Check a
re-download against it before replacing anything here:

    Get-FileHash .\VirtualDisplayDriver-x86.Driver.Only.zip -Algorithm SHA256

## Do not edit anything in this folder

`mttvdd.cat` is a signed catalog holding a hash of every other file, `MttVDD.inf`
included. Windows checks the package against it at install time, so an edit —
renaming the device, tidying the `.inf`, rewriting line endings — voids the
signature. Windows then refuses the package, or wants the machine put into
test-signing mode.

That is also why the publisher Windows names in its trust dialog is theirs and
not Tain's. It is their driver, signed by them, installed as they signed it.

Tain reads the class, hardware id and device name *out of* the `.inf` rather
than asserting any of them, so replacing this folder with a newer release needs
no code change.

## What the `.inf` declares

    ClassGUID   = {4D36E968-E325-11CE-BFC1-08002BE10318}   ; Display
    Section     = [Standard.NTamd64]                        ; x64 only
    Hardware id = Root\MttVDD          ; the device Tain creates
                  MttVDD               ; a second id, for their sample app
    DeviceName  = "Virtual Display Driver"
    DriverVer   = 12/24/2024, 11.30.4.434
    PnpLockdown = 1

Two things worth knowing, both found by reading the file rather than the
release notes:

- **`NTamd64` settles the architecture question.** The asset is named `x86`,
  which suggested a 32-bit build; the `.inf` has one architecture section and
  it is amd64. The name is a misnomer, and the ARM64 asset is the other build.
- **There are two hardware ids on consecutive lines.** `Root\MttVDD` is the
  root-enumerated device, which is the one to create. The bare `MttVDD` belongs
  to their `IddSampleApp.exe`. Registering against the wrong one produces a
  device nothing enumerates, which reports success.

The file is UTF-16 with a byte-order mark. Read as UTF-8 it matches nothing.

## How the driver is told to present a monitor

Installing it registers adapters; it presents no monitor until asked. That is
not a Win32 call — it is this driver's own control channel, read out of
`MttVDD.dll`:

- Named pipe `\\.\pipe\MTTVirtualDisplayPipe`.
- `SETDISPLAYCOUNT` is the command that creates and destroys monitors. The
  driver writes the number into its settings and restarts itself: *"Display
  Count Changed, Restarting Driver"*.
- `PING`, `GETSETTINGS`, `RELOAD_DRIVER`, `GETALLGPUS`, `GETASSIGNEDGPU`,
  `VDDPATH`, and option verbs this project has no reason to use.

Settings live in `vdd_settings.xml` at `C:\VirtualDisplayDriver\`, unless
`HKLM\SOFTWARE\MikeTheTech\VirtualDisplayDriver` points elsewhere. `<monitors>
<count>` is what `SETDISPLAYCOUNT` writes; `<resolutions>` is the mode list each
presented monitor offers.

Everything after a monitor exists — onto the desktop, at a size, and away again
— is `tain_core::virtual_display`, which works with any driver's monitor and
knows nothing about this one.

## Known upstream issue

The project's tracker has had the **ARM64** package refused for signing where
x64 was fine. Worth testing before offering the install button on an ARM
machine.
