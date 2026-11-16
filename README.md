# wispr-flow-linux-helper

> Clean-room native Rust companion binary (`wispr-flow-linux-helper`) and GNOME Shell window bridge extension for **Wispr Flow on Linux** — maintained as part of [`mohammed160209/wispr-flow-linux`](https://github.com/mohammed160209/wispr-flow-linux).

Wispr Flow's upstream Windows and macOS releases delegate OS-level hooks to a small native helper process. This repository implements that exact binary for **Arch Linux / CachyOS, Ubuntu/Debian, Fedora**, and other modern Linux desktops, plus **native PipeWire system audio capture** so Notetaker meetings work cleanly without spawning heavy external recorders.

---

## Core Capabilities

| Subsystem | Implementation | Platforms / Compositors |
| :--- | :--- | :--- |
| **PipeWire System Audio (`--capture-system-audio`)** | In-process `dlopen("libpulse-simple.so.0")` capturing `@DEFAULT_MONITOR@` at `16 kHz` mono `s16le` (`1280`-byte / `40 ms` fragments) wrapped in `1,304`-byte `WAF2` frames over `stdout` + `endpoint_opened` on `stderr` | PipeWire (`pipewire-pulse`), PulseAudio |
| **Notetaker Input Modes** | Reads `$XDG_CONFIG_HOME/Wispr Flow/notetaker-input-mode.json` live (`both`, `system_only`, `mic_only`) & reconnects automatically if the active output sink changes (speakers → headphones/Bluetooth) | Any Linux PipeWire setup |
| **Global Keystroke Injection (`PasteText`, `SimulateKeyPress`)** | In-process `/dev/uinput` virtual keyboard + `wl-clipboard` on Wayland; `XTEST` + `xclip`/`xsel` on X11 | Wayland (GNOME, KDE, wlroots/Hyprland/Sway) & X11 |
| **Global Hotkey Monitor** | `/dev/input/event*` evdev listener on Wayland, `XInput2` on X11 | Wayland & X11 |
| **Active Window Context (`GetActiveAppInfo`)** | Bundled GNOME Shell ESM extension ([`wispr-flow-window-bridge@wispr.flow`](src/backend/gnome_extension/extension.js)) over D-Bus; KDE KWin script bridge over D-Bus; `_NET_ACTIVE_WINDOW` on X11 | GNOME 45–50+, KDE Plasma, X11 |
| **Floating Pill Stacking & Clipping Fixes** | Keeps `Status` / `Flow Status Indicator` overlays (`_NET_WM_WINDOW_TYPE` / `Meta.WindowType.UTILITY`) above fullscreen/focused windows up to `1600x1600` (`512x614` @ HiDPI) with Dash to Dock `intellihide-ignore-wispr-overlay` compatibility | GNOME Mutter (Wayland & XWayland) |

---

## PipeWire System Audio Capture (`--capture-system-audio`)

Upstream Wispr Flow Notetaker expects Windows' native `WASAPI` loopback helper framing (`endpoint_opened` on `stderr`, followed by continuous `WAF2` packets on `stdout`). On Linux, [`src/system_audio.rs`](src/system_audio.rs) satisfies that contract with zero extra child processes:

```text
[PipeWire Default Sink Monitor (@DEFAULT_MONITOR@)]
        │  16 kHz · mono · s16le · 40 ms (1280 B)
        ▼
[wispr-flow-linux-helper --capture-system-audio]
        │  header: "WAF2" + len(1280) + seq(u64) + ticks_100ns(i64)
        ▼  stdout (1304-byte frames)
[Wispr Flow Electron Notetaker Engine]  ◄── mixed in parallel with Microphone
```

You can test the standalone PipeWire capture stream directly at any time:

```bash
(sleep 1.5) | ./target/release/wispr-flow-linux-helper --capture-system-audio > /tmp/waf2.bin
# stderr prints: endpoint_opened
# /tmp/waf2.bin contains exact 1,304-byte WAF2 frames
```

---

## Building From Source

### Requirements
- **Rust toolchain** (`rustup` or Arch `rust` package)
- Runtime libraries only (no extra `-dev` C headers required thanks to pure-Rust `x11rb`/`zbus` and runtime `dlopen` for `libpulse-simple.so.0`):
  - **Arch Linux / CachyOS:** `sudo pacman -S --needed pipewire pipewire-pulse wl-clipboard xclip`
  - **Debian / Ubuntu:** `sudo apt install pipewire-pulse libpulse0 wl-clipboard xclip`
  - **Fedora:** `sudo dnf install pipewire-pulseaudio pipewire-libs wl-clipboard xclip`

### Build & Test

```bash
git clone https://github.com/mohammed160209/wispr-flow-helper.git
cd wispr-flow-helper

cargo test
cargo build --release
# Output binary: target/release/wispr-flow-linux-helper
```

### Pairing with `wispr-flow-linux`

Either let [`wispr-flow-linux`](https://github.com/mohammed160209/wispr-flow-linux) fetch the pinned release asset automatically, or pass your freshly compiled binary directly:

```bash
HELPER_BIN="$PWD/target/release/wispr-flow-linux-helper" \
  ../wispr-flow-linux/scripts/build-linux.sh
```

---

## Repository Structure

```text
src/
├── main.rs                      # CLI flags (--capture-system-audio, IPC dispatcher)
├── system_audio.rs              # In-process PipeWire/PulseAudio WAF2 monitor capture
├── proto.rs                     # Framed pipe IPC protocol (stdin commands / fd 3 events)
├── keymap.rs                    # Windows VK -> X11 keysym / Linux evdev mappings
├── capture/                     # Global hotkey readers (evdev, XInput2)
└── backend/
    ├── mod.rs                   # Runtime compositor detection (Wayland / X11 / GNOME / KDE)
    ├── gnome.rs                 # GNOME Shell extension auto-installer & D-Bus bridge client
    ├── gnome_extension/         # Bundled GNOME 45–50+ Window Bridge extension.js
    ├── kwin.rs                  # KDE Plasma active-window D-Bus bridge
    ├── uinput.rs                # In-process /dev/uinput virtual keyboard injector
    ├── wl_clipboard.rs          # Native Wayland clipboard handler
    └── x11.rs                   # X11 / XTEST backend
```

---

## License & Attribution

Released into the public domain under the [Unlicense](UNLICENSE). Originally forked from [wispr-flow-linux/helper](https://github.com/wispr-flow-linux/helper) and actively maintained by [Mohammed Sakarwala](https://github.com/mohammed160209) with PipeWire Notetaker loopback capture and GNOME/XWayland overlay improvements.
