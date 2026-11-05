<p align="center">
  <img src="assets/oxidezap.svg" width="112" alt="OxideZap logo — a lightweight WhatsApp Web desktop client for Linux">
</p>

<h1 align="center">OxideZap</h1>

<p align="center">
  <strong>A lightweight, native WhatsApp Web desktop client for Linux — built with Rust and GPUI.</strong>
</p>

<p align="center">
  <a href="https://github.com/artur282/oxidezap/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/artur282/oxidezap/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/artur282/oxidezap/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/artur282/oxidezap?label=release"></a>
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/github/license/artur282/oxidezap"></a>
  <img alt="Platform: Linux" src="https://img.shields.io/badge/platform-Linux-informational">
  <img alt="Built with Rust and GPUI" src="https://img.shields.io/badge/built%20with-Rust%20%2B%20GPUI-orange">
</p>

> **Unofficial, independent project.** OxideZap is not affiliated with, endorsed by, or sponsored by WhatsApp or Meta. It simply loads the official [`web.whatsapp.com`](https://web.whatsapp.com) interface in the system WebKitGTK engine and wraps it in a native Rust shell.

## What is OxideZap?

OxideZap is an open-source **WhatsApp Web desktop client for Linux**. It gives WhatsApp Web the desktop conveniences a browser tab cannot offer — a system tray, native notifications, persistent accounts, an app lock, global shortcuts, and a native settings UI — while keeping the memory footprint of the native shell small.

The native shell is written in **Rust** and rendered with **[GPUI](https://gpui.rs)**, the GPU-accelerated UI framework behind [Zed](https://zed.dev). The WhatsApp Web page itself runs in **WebKitGTK**, the web engine already shipped by your Linux distribution, embedded into the GPUI window. No Electron, no bundled Chromium, no Node.js runtime.

> **Status:** early development. The architecture and feature set below describe the v0.1 target. Check the [roadmap](#roadmap) for what is implemented.

## Why GPUI?

Tauri is a webview wrapper: the application UI is HTML/CSS/JS rendered by the same webview that renders the page. OxideZap takes a different route. The application chrome — titlebar, account switcher, settings, lock screen, tray menu, notifications — is **native GPUI**, drawn by the GPU. Only the WhatsApp Web content area is a webview.

That split matters:

- The native shell is **small and fast**, and it does not ship a second browser engine.
- The UI is **real Rust**, not a web page pretending to be a desktop app.
- The webview is isolated to the content area, so a WhatsApp Web update cannot break the native chrome.
- GPUI gives us GPU-accelerated rendering, a modern layout model, and a single Rust codebase.

## Features

### Accounts

- **Multiple WhatsApp accounts at once** — every account gets its own window and a fully isolated WebKit data store (cookies, local storage, IndexedDB, caches).
- **Add, rename, and remove accounts** from the native settings window.
- **Per-account unread counts**, notifications, and tray entries.
- **Persistent login** — scan the QR code once and stay signed in across restarts.

### Desktop integration

- **System tray** with show/hide, account switching, settings, lock, and quit.
- **Close to tray** — closing a window hides it instead of quitting.
- **Unread badge** in the tray tooltip and, on supported desktops, on the launcher icon.
- **Native notifications** for new messages, with the sender and message preview.
- **Global show/hide shortcut** (default `Ctrl+Shift+W`), configurable in Settings.
- **Single instance** — launching OxideZap again focuses the running instance.
- **Launch at login** via an XDG autostart entry.
- **`whatsapp://` link handling** — click-to-chat links open in OxideZap.
- **External links open in your browser** — web, email, and phone links leave the app; `wa.me` and group invite links stay inside.
- **Download feedback** — downloads are saved to `~/Downloads` and confirmed with a native toast.
- **Remembers window size and position** per account.

### Comfort

- **Display size** — zoom WhatsApp Web from 80% to 150% without touching the rest of the UI.
- **Light, dark, and system themes** for the native shell.
- **App lock** — Argon2id password, lock on launch, on hide, or after an idle timeout.
- **Voice messages** work out of the box (microphone permission is handled by WebKitGTK).
- **Drag and drop** files and images onto a chat to attach them.

## Architecture

```text
┌──────────────────────────────────────────────────────────────────┐
│ GPUI process                                                     │
│                                                                  │
│  ┌────────────────────────┐      ┌────────────────────────────┐  │
│  │ GPUI main thread       │      │ GTK / WebKit thread        │  │
│  │                        │      │                            │  │
│  │  • app shell           │      │  • GtkWindow (RGBA)        │  │
│  │  • titlebar            │      │  • WebKitWebView           │  │
│  │  • settings / lock     │◀────▶│  • isolated data store     │  │
│  │  • tray / notifications│ IPC  │  • JS bridge               │  │
│  │  • global shortcuts    │      │  • downloads / links       │  │
│  └───────────┬────────────┘      └──────────────┬─────────────┘  │
│              │ X11 reparent                      │                │
│              └──────────────┬─────────────────────┘                │
│                             ▼                                    │
│              ┌──────────────────────────────┐                    │
│              │ GPUI window (32-bit visual)  │                    │
│              │  ┌────────────────────────┐  │                    │
│              │  │ native titlebar (GPUI) │  │                    │
│              │  ├────────────────────────┤  │                    │
│              │  │ WebKitGTK web content  │  │                    │
│              │  └────────────────────────┘  │                    │
│              └──────────────────────────────┘                    │
└──────────────────────────────────────────────────────────────────┘
```

- **GPUI** owns the native shell and the main event loop.
- **WebKitGTK** owns the WhatsApp Web page in a dedicated thread with its own GLib loop.
- The webview is embedded into the GPUI window with X11 reparenting, so it behaves like a native child window: it clips, moves, and resizes with the shell.
- On a Wayland session the app runs through **XWayland**, because embedding a foreign GTK surface into a Wayland client is not portable. Native Wayland is tracked in the roadmap.
- Accounts are isolated with separate `WebKitWebsiteDataManager` data and cache directories.

## Requirements

| Component | Requirement |
| --- | --- |
| OS | Linux (X11 or XWayland) |
| WebKitGTK | `webkit2gtk-4.1` **≥ 2.46.1** (older versions can hang WhatsApp's QR login) |
| GTK | GTK 3 |
| GPU | Vulkan-capable driver (GPUI uses Vulkan through `blade`) |
| Rust | Stable, 1.85+ |

### Debian / Ubuntu / Linux Mint

```bash
sudo apt install build-essential pkg-config \
  libwebkit2gtk-4.1-dev libgtk-3-dev libjavascriptcoregtk-4.1-dev \
  libxkbcommon-dev libvulkan-dev libfontconfig1-dev libfreetype-dev \
  libx11-dev libxcb1-dev libssl-dev
```

### Fedora

```bash
sudo dnf install gcc gcc-c++ make pkgconf-pkg-config \
  webkit2gtk4.1-devel gtk3-devel javascriptcoregtk4.1-devel \
  libxkbcommon-devel vulkan-loader-devel fontconfig-devel freetype-devel \
  libX11-devel libxcb-devel openssl-devel
```

### Arch

```bash
sudo pacman -S base-devel pkgconf webkit2gtk-4.1 gtk3 \
  xkbcommon vulkan-icd-loader fontconfig freetype2 libx11 libxcb openssl
```

## Build from source

```bash
git clone https://github.com/artur282/oxidezap.git
cd oxidezap
cargo build --release
./target/release/oxidezap
```

For a quick development loop:

```bash
cargo run
```

## Install

```bash
./install.sh
```

The installer builds the release binary, installs it to `~/.local/bin`, adds the desktop entry, the icon, and the `whatsapp://` URL handler. Use `PREFIX=/usr/local sudo ./install.sh` for a system-wide install.

## Usage

| Action | How |
| --- | --- |
| Show / hide | Tray icon, or `Ctrl+Shift+W` |
| Add an account | Settings → Accounts → **Add account** |
| Switch accounts | Tray menu, or the account switcher in the titlebar |
| Lock the app | Tray → **Lock now**, or `Ctrl+Shift+L` |
| Zoom WhatsApp Web | Settings → Appearance → Display size |
| Open settings | Titlebar gear, or tray → **Settings** |

## Roadmap

- [x] Repository, architecture, and build pipeline
- [ ] GPUI shell with embedded WebKitGTK content
- [ ] Persistent single account and QR login
- [ ] Multi-account isolation
- [ ] System tray and close-to-tray
- [ ] Native notifications and unread badge
- [ ] Settings window and display zoom
- [ ] App lock (Argon2id)
- [ ] Global shortcut and single instance
- [ ] Downloads, external links, and `whatsapp://`
- [ ] `.deb` and AppImage packaging
- [ ] Native Wayland embedding
- [ ] Biometric unlock (polkit / fprintd)

## Limitations

- **X11 only for embedding.** On Wayland, OxideZap runs under XWayland. Native Wayland support requires a different compositing path and is on the roadmap.
- **WhatsApp Web is a web app.** Total memory is dominated by the page itself; OxideZap keeps the native shell small, not the web content.
- **Voice and video calls** depend on WebKitGTK's WebRTC support and are not guaranteed on every distribution.
- **Not affiliated with WhatsApp or Meta.** Use it with your own account and at your own risk.

## Contributing

Issues and pull requests are welcome. Please run `cargo fmt`, `cargo clippy -- -D warnings`, and `cargo test` before opening a PR. For larger changes, open an issue first so we can agree on the approach.

## License

MIT — see [LICENSE](LICENSE).
