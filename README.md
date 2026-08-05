# Caybul

Cross-platform file transfer over a wire, with no setup.

Plug two computers together, open Caybul on both, drop your files in. It picks the
fastest path the cable can do and moves them. Works between macOS, Windows and
Linux in any combination.

## Cable Support

Which cables carry a link between which systems. Hover an icon for detail.

| | <img src="docs/icons/apple.svg" height="15" alt=""> Mac | <img src="docs/icons/windows.svg" height="15" alt=""> Windows | <img src="docs/icons/linux.svg" height="15" alt=""> Linux |
|---|---|---|---|
| <img src="docs/icons/apple.svg" height="15" alt=""> **Mac** | <img src="docs/icons/thunderbolt.svg" height="16" alt="Thunderbolt" title="Thunderbolt / USB4 — 10–40 Gbps, any two systems"> <img src="docs/icons/ethernet.svg" height="16" alt="Ethernet" title="Ethernet — 1–10 Gbps, any two systems (direct or USB-C adapter)"> <img src="docs/icons/usb-c.svg" height="16" alt="USB-C" title="USB-C — Mac-to-Mac only, ~480 Mbps (USB 2)"> | <img src="docs/icons/thunderbolt.svg" height="16" alt="Thunderbolt" title="Thunderbolt / USB4 — 10–40 Gbps"> <img src="docs/icons/ethernet.svg" height="16" alt="Ethernet" title="Ethernet — 1–10 Gbps"> | <img src="docs/icons/thunderbolt.svg" height="16" alt="Thunderbolt" title="Thunderbolt / USB4 — 10–40 Gbps"> <img src="docs/icons/ethernet.svg" height="16" alt="Ethernet" title="Ethernet — 1–10 Gbps"> |
| <img src="docs/icons/windows.svg" height="15" alt=""> **Windows** | <img src="docs/icons/thunderbolt.svg" height="16" alt="Thunderbolt" title="Thunderbolt / USB4 — 10–40 Gbps"> <img src="docs/icons/ethernet.svg" height="16" alt="Ethernet" title="Ethernet — 1–10 Gbps"> | <img src="docs/icons/thunderbolt.svg" height="16" alt="Thunderbolt" title="Thunderbolt / USB4 — 10–40 Gbps"> <img src="docs/icons/ethernet.svg" height="16" alt="Ethernet" title="Ethernet — 1–10 Gbps"> | <img src="docs/icons/thunderbolt.svg" height="16" alt="Thunderbolt" title="Thunderbolt / USB4 — 10–40 Gbps"> <img src="docs/icons/ethernet.svg" height="16" alt="Ethernet" title="Ethernet — 1–10 Gbps"> |
| <img src="docs/icons/linux.svg" height="15" alt=""> **Linux** | <img src="docs/icons/thunderbolt.svg" height="16" alt="Thunderbolt" title="Thunderbolt / USB4 — 10–40 Gbps"> <img src="docs/icons/ethernet.svg" height="16" alt="Ethernet" title="Ethernet — 1–10 Gbps"> | <img src="docs/icons/thunderbolt.svg" height="16" alt="Thunderbolt" title="Thunderbolt / USB4 — 10–40 Gbps"> <img src="docs/icons/ethernet.svg" height="16" alt="Ethernet" title="Ethernet — 1–10 Gbps"> | <img src="docs/icons/thunderbolt.svg" height="16" alt="Thunderbolt" title="Thunderbolt / USB4 — 10–40 Gbps"> <img src="docs/icons/ethernet.svg" height="16" alt="Ethernet" title="Ethernet — 1–10 Gbps"> |

<sub><img src="docs/icons/thunderbolt.svg" height="14" alt=""> Thunderbolt / USB4 &nbsp;&nbsp; <img src="docs/icons/ethernet.svg" height="14" alt=""> Ethernet &nbsp;&nbsp; <img src="docs/icons/usb-c.svg" height="14" alt=""> USB-C</sub>

With no cable, Caybul falls back to Wi-Fi or the local network and asks for a
confirmation code first.

Why plain USB-C is nearly all blank: USB needs one side to be the host and the
other the device, and a PC's USB port can only ever be the host. Two Macs get
around it because a Mac can play the device end; a PC can't, so any pairing with
a PC on plain USB-C simply doesn't come up. Two things that follow from this:

- A cable rated "5 Gbps" or "10 Gbps" only reaches that between a computer and a
  device. Two computers is a different case and the rating doesn't carry over.
- USB-C is a connector, not a protocol. A Thunderbolt cable and a charge-only
  cable can look identical. Thunderbolt cables have a lightning bolt on them.

Caybul reads the link type and reports the speed it measured, so you can tell
which case you're in.


## How it works

Each app announces itself on the wire and the two find each other, no addresses
to type. A wired connection counts as consent and connects directly; over Wi-Fi
it shows a code on both screens first. Files go out in chunks across several
streams, get checksummed on arrival, and resume from where they stopped if the
connection drops.

## Install

Prebuilt macOS, Windows and Linux downloads are on the
[releases page](../../releases). The builds aren't signed, so the first launch
trips Gatekeeper or SmartScreen. Open it anyway.

From source:

    cargo build --release

`caybul-gui` is the app. `caybul` is the command line:

    caybul links      # what's connected, and how fast
    caybul receive    # wait for files
    caybul send FILE  # send to whoever's listening

## Build

Uses Rust toolchain. Linux needs GTK and a few xcb libraries for the window and file dialog:

    sudo apt install libgtk-3-dev libxkbcommon-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev

Three crates: `caybul-core` (detection, discovery, transfer), `caybul-cli`,
`caybul-gui`.

MIT licensed.
