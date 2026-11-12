<p align="center">
  <img src="assets/oxidezap.svg" width="112" alt="OxideZap logo">
</p>

<h1 align="center">OxideZap</h1>

<p align="center">
  <strong>A lightweight, native WhatsApp Web desktop client for Linux, built with Rust and GPUI.</strong>
</p>

<p align="center">
  <a href="https://github.com/artur282/oxidezap/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/artur282/oxidezap/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/artur282/oxidezap/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/artur282/oxidezap?label=release"></a>
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/github/license/artur282/oxidezap"></a>
  <img alt="Platform: Linux" src="https://img.shields.io/badge/platform-Linux-informational">
  <img alt="Built with Rust and GPUI" src="https://img.shields.io/badge/built%20with-Rust%20%2B%20GPUI-orange">
</p>

> **Unofficial, independent project.** OxideZap is not affiliated with, endorsed by, or sponsored by WhatsApp or Meta. It loads the official [`web.whatsapp.com`](https://web.whatsapp.com) interface in the system WebKitGTK engine and wraps it in a native Rust shell.

## What is OxideZap?

OxideZap is an open-source **WhatsApp Web desktop client for Linux**. It gives WhatsApp Web the desktop conveniences a browser tab cannot offer: a system tray, native notifications, multiple isolated accounts, an application lock, a global show/hide shortcut, and a native settings window.

The native shell is written in **Rust** and rendered with **[GPUI](https://gpui.rs)**, the GPU-accelerated UI framework behind [Zed](https://zed.dev). The WhatsApp Web page itself runs in **WebKitGTK**, the web engine already shipped by most Linux distributions. There is no Electron, no bundled Chromium, and no Node.js runtime.

OxideZap is early-stage software. Version 0.1.0 implements the features described below, but the project is still young and Linux with X11 or XWayland is the only supported platform.

## Why GPUI?

Tauri is a webview wrapper: the application UI is HTML/CSS/JS rendered by the same webview that renders the page. OxideZap takes a different route. The application chrome -- titlebar, settings, lock screen, tray menu, notifications -- is **native GPUI**, drawn by the GPU. Only the WhatsApp Web content area is a webview.

That split matters:

- The native shell is **small and fast**, and it does not ship a second browser engine.
- The UI is **real Rust**, not a web page pretending to be a desktop app.
- The webview is isolated to the content area, so a WhatsApp Web update cannot break the native chrome.
- GPUI provides GPU-accelerated rendering, a modern layout model, and a single Rust codebase.

## Features

### Accounts

- **Multiple WhatsApp accounts at once.** Each account gets its own GPUI window and its own WebKit data store (cookies, local storage, IndexedDB, caches) under `~/.local/share/oxidezap/accounts/<uuid>/`.
- **Add, rename, remove, and open accounts** from the native settings window.
- **Persistent login.** Scan the QR code once and stay signed in across restarts.
- **Per-account unread counts**, notifications, and tray entries.
- **Remembered window bounds** per account.
- **Private on-disk data.** The settings file and each account's WebKit profile are created with `0600`/`0700` permissions. Existing installations are migrated on startup without following symlinks; the process runs with a restrictive `umask` so WebKit's cookies, local storage, IndexedDB, and caches inherit private modes.
- **Storage manager.** Settings > Storage lists how much data and cache every account uses, the total, the free disk space, and the time of the last cleanup. Clear the cache without signing out, sign out while keeping the account, or delete an account's data after typing its name. The scan runs on a worker thread and every cleanup closes and recreates the affected webview so WebKit never writes into a directory that is being deleted.
- **Low-memory mode (off by default).** Settings > Performance can release an account's WebKit view and web process after it stays hidden for 15, 30, or 60 minutes. The account, its profile, cookies, and WhatsApp login stay on disk; showing the account again recreates the view and reloads WhatsApp Web with a native loading state. The policy never releases a visible account, the most recently active account (unless you turn that guard off), an account with an active download, call, or recording, or any account while the app is locked or quitting. A tray entry and **Resume all suspended accounts** command show what is currently released, and **Release hidden accounts now** performs the sweep on demand.
- **Performance and battery saver (off by default).** Settings > Performance offers **Automatic**, **Normal**, and **Battery saver**. Automatic reads `/sys/class/power_supply` and enables the saver only while the machine is on battery; systems without a readable battery stay in normal mode. Battery saver pauses WhatsApp Web animation and CSS transitions, checks the connection/QR state every 5 s instead of 2 s, slows native overlay synchronization, and, when low-memory mode is already enabled, shortens the suspension delay to 15 minutes and sweeps more often. Notifications, downloads, calls, the app lock, and security checks stay enabled.

### Native shell

- **Custom GPUI titlebar** with the account name, unread badge, command palette, settings, lock, minimize, maximize, and close controls, plus drag and edge-resize support.
- **Command palette.** `Ctrl+K` in the native window, or the titlebar palette button, opens a keyboard-first command palette: type to filter, `Up`/`Down` to select, `Enter` to run, `Esc` to close. It exposes existing actions only: show/hide, show or hide all windows, settings, add or switch accounts, download center or folder, storage folder, lock, Do Not Disturb, theme, zoom, retry connection, and quit. `Ctrl+1`..`Ctrl+9` switch accounts while the native window has focus, and an optional global palette shortcut can be configured in Settings -> General.
- **Clipboard paste.** Copy a file, photo, video, or a screenshot/image from the clipboard and press `Ctrl+V` in a chat: OxideZap injects it into WhatsApp's attachment flow. Screenshots and image data are encoded as PNG and sent as photos, never as stickers. Files over 64 MB should use the attach button.
- **Media action menu (experimental).** Right-click a normal image, video, audio, or document attachment that is already visible in a chat to open a small OxideZap menu: **Save to Downloads**, **Open in system viewer**, and, for images, **Copy image**. The menu is injected as a WebKit user script and only inspects the element under the pointer; it never scans chats or observes mutations. View-once, stickers, avatars, and media from non-WhatsApp origins are never offered. Saved files go through the normal download pipeline, so they appear in the Download Center with account, date, and size. Disable the menu in Settings > General. `Esc` closes it and the arrow keys move between its items.
- **Settings window** with Accounts, Storage, Appearance, Notifications, Network, General, Performance, Security, and About sections.
- **English and Spanish native UI.** The shell detects the desktop locale and can be overridden in Settings -> General -> Interface language. The change applies to open native windows without a restart. WhatsApp Web keeps its own language and is never modified. See [Interface language](#interface-language).
- **Diagnostics report.** Settings -> About can preview, copy, or save a privacy-scrubbed text report with the OxideZap, GTK, WebKitGTK, distribution, kernel, CPU, RAM, and session details; settings states; runtime counts; redacted paths; and the last warnings and errors from the current session. It never includes chats, account names or ids, cookies, proxy credentials, the app-lock hash, tokens, or full home paths.
- **First-run guide.** A five-page native window explains the WhatsApp Web QR login, account isolation, tray and close behavior, notifications, the app lock, view-once limits, and where data lives. It opens once on a new installation, is never modal, and can be skipped, closed, or reopened from Settings -> General.
- **Themes**: System, Light, and Dark for the native shell.
- **WhatsApp zoom** from 80% to 150%.
- **Composer spell checking.** Choose Automatic (desktop locale), Spanish, English, or Disabled, and toggle **Correct while I type**. WebKitGTK checks locally with the system hunspell dictionaries; no text is sent to external services.
- **Notification toggle** in Settings.
- **Close to tray**, **launch at login**, **remember bounds**, a **configurable global show/hide shortcut**, and a configurable **boss key** that hides every window immediately and restores them on the next press.
- **App lock** with an Argon2id password. Lock on launch, on hide, on system suspend, when the desktop screen locks (best effort), or after an idle timeout; lock now from the tray or Settings; reset the password from Settings. While locked, the privacy screen hides every account window and the settings window, and only the lock window remains. Locking never closes WhatsApp Web, so the session and unsent drafts survive.
- **Settings backup and restore.** Export the configuration to a readable JSON file, import it on another installation with a change summary and confirmation, or reset preferences to their defaults. Exports never contain account sessions, cookies, chat history, or the app-lock password hash; every import and reset writes a dated backup under `~/.config/oxidezap/backups/` first.
- **Bounded crash recovery.** If WebKit terminates, OxideZap retries with a 1 s, 3 s, and 10 s backoff, then shows a native error state with a manual **Retry now** button instead of reloading forever.
- **Connection status and automatic reconnect.** Each account titlebar shows a discreet native indicator: **Connected**, **Offline**, **Connecting…**, or **Reconnecting…**. A small injected WebKit user script reports the browser's `online`/`offline` events and whether the QR login screen is showing; native load-failure signals and a 30 s load timeout are the fallback. When the network returns while the page is in an error state, OxideZap reloads it with a bounded 2 s, 5 s, 15 s, 30 s, and 60 s backoff (at most five attempts). It never reloads while a download is active or while the QR/login screen is showing; use the native **Retry now** button in those cases. Optional native notifications report offline and reconnection events and respect Do not disturb and per-account mute.
- **Responsive foreground.** Window tagging, settings persistence, notifications, external launchers, and clipboard file reads run away from the GPUI frame loop; GTK calls stay on the GTK thread.

### Desktop integration

- **System tray** through StatusNotifierItem, with show/hide, account list, add account, settings, lock, per-account window modes, and quit.
- **Unread badge** in the tray icon (`1`-`9`, `9+`, and a dimmed DND state), in the per-account tooltip breakdown, and, on supported desktops, on the launcher icon through the Unity LauncherEntry D-Bus interface.
- **Native notifications** for new messages, with sender and message preview, a global Do Not Disturb switch with timed and recurring quiet hours, and per-account mute/sound controls.
- **Single instance** through a Unix socket. Launching OxideZap again forwards the request to the running instance. The socket is created with `0600` permissions and never listens on the network; CLI actions only pass validated account ids, chat targets, local file paths, and built-in action names.
- **Global show/hide shortcut** through X11 `XGrabKey`, default `Ctrl+Shift+W`, configurable in Settings. A second configurable shortcut (the **boss key**, default `Ctrl+Shift+H`) hides every window immediately and restores them on the next press. Both are X11/XWayland-only.
- **Lock on session events.** OxideZap listens for logind's `PrepareForSleep` signal and for `ActiveChanged` from the desktop ScreenSaver D-Bus service, and locks with the same privacy screen as a manual lock. Both integrations are best effort: desktops and sandboxes that do not expose the signals fall back to manual, hide, and idle locking.
- **Launch at login** through an XDG autostart entry at `~/.config/autostart/oxidezap.desktop`.
- **Link handling.** External `http(s)`, `mailto:`, and `tel:` links open in the system browser or application. `wa.me`, `chat.whatsapp.com`, and `web.whatsapp.com` links stay inside OxideZap. WhatsApp's internal `whatsapp.net` utility pages (such as the Flows cache manager) stay in the webview and never open a system browser tab. `whatsapp://` URLs are translated to their WhatsApp Web equivalents.
- **Native Download Center.** WebKit downloads are tracked with live progress, status, account, size, timestamps, and destination in a bounded 500-entry history. Media saved from the media action menu is recorded the same way, so the Download Center doubles as the downloaded-media gallery per account. Cancel an active transfer, retry a failed one when WebKit exposed an HTTP(S) source, open the file or its folder, copy the path, or remove finished rows. Set a custom base folder and optionally organize files into per-account and `YYYY-MM` subfolders. Names are sanitized, collisions get a numbered suffix, and existing files are never overwritten. Open it from the tray or Settings.
- **CLI control** with `--toggle`, `--hide`, `--lock`, `--quit`, `--account`, `--chat`, `--share`, `--dnd`, `--downloads`, `--status`, and `--list-accounts`, plus URL forwarding to a running instance.

## Requirements

| Component | Requirement |
| --- | --- |
| OS | Linux with X11 or XWayland |
| WebKitGTK | `webkit2gtk-4.1` **>= 2.46.1** (older versions can hang WhatsApp's QR login) |
| GTK | GTK 3 |
| GPU | Vulkan-capable GPU and driver (GPUI uses Vulkan through Blade) |
| Rust | Stable, 1.85 or newer |

The tray requires a StatusNotifier host. Native notifications require a notification daemon. Voice and video calls, as well as voice-note recording, depend on WebKitGTK's WebRTC and portal support and may not work on every distribution.

Spell checking additionally requires hunspell dictionaries for the languages you select, for example `hunspell-es` and `hunspell-en-us` on Debian-based distributions. WebKitGTK silently skips a language with no installed dictionary.

The Flatpak and AppImage artifacts do not change these requirements. The Flatpak runtime supplies WebKitGTK 2.54.1 and GTK 3; the AppImage uses the host's WebKitGTK and GTK 3 and does not bundle them.

### Debian, Ubuntu, and Linux Mint

On Ubuntu 24.04 or a compatible distribution:

```bash
sudo apt install build-essential pkg-config clang cmake ninja-build \
  libwebkit2gtk-4.1-dev libjavascriptcoregtk-4.1-dev libgtk-3-dev \
  libxkbcommon-dev libfontconfig1-dev libfreetype-dev libvulkan-dev \
  libx11-dev libxcb1-dev libwayland-dev libssl-dev \
  mesa-vulkan-drivers
```

On other distributions, install the equivalent WebKitGTK 4.1, GTK 3, X11, XCB, Vulkan, fontconfig, FreeType, and OpenSSL development packages.

## Build from source

```bash
git clone https://github.com/artur282/oxidezap.git
cd oxidezap
cargo build --release
./target/release/oxidezap
```

The release binary is `target/release/oxidezap`. The first build compiles GPUI, WebKitGTK bindings, and the rest of the dependency tree, so it can take several minutes.

For a faster development loop:

```bash
cargo run
```

## Install

The installer is user-local and does not require root. Release tags also publish [Flatpak](#flatpak) and [AppImage](#appimage) bundles; `install.sh` remains the recommended path for development and for a native, un-sandboxed install.

```bash
./install.sh
```

It installs:

- the binary to `~/.local/bin/oxidezap`;
- the desktop entry to `~/.local/share/applications/oxidezap.desktop`;
- the icon to `~/.local/share/icons/hicolor/scalable/apps/oxidezap.svg`;
- AppStream metadata to `~/.local/share/metainfo/`;
- the `whatsapp://` URL handler.

If `target/release/oxidezap` does not exist, the installer builds it with `cargo build --release`. Set `PREFIX` to install somewhere else:

```bash
PREFIX="$HOME/.local" ./install.sh
```

The script never invokes `sudo`. If you install into a system prefix such as `/usr/local`, run it with the permissions that prefix requires.

To remove the installed files without deleting your accounts or settings:

```bash
./install.sh --uninstall
```

### Manual install

```bash
install -Dm755 target/release/oxidezap ~/.local/bin/oxidezap
install -Dm644 assets/oxidezap.svg \
  ~/.local/share/icons/hicolor/scalable/apps/oxidezap.svg
install -Dm644 packaging/oxidezap.desktop \
  ~/.local/share/applications/oxidezap.desktop
```

The packaged desktop entry uses `Exec=oxidezap %u`, so `~/.local/bin` must be in your `PATH`. The installer rewrites it to an absolute path instead. After a manual install, register the URL handler and refresh the desktop database:

```bash
xdg-mime default oxidezap.desktop x-scheme-handler/whatsapp
update-desktop-database ~/.local/share/applications
```

### Flatpak

Each release publishes `oxidezap-vVERSION-x86_64.flatpak`. The bundle targets the GNOME 51 runtime and exports the desktop entry, icon, AppStream metadata, and the `whatsapp://` handler.

```bash
flatpak install --user ./oxidezap-v0.1.0-x86_64.flatpak
# If the desktop did not register the handler automatically:
xdg-mime default io.github.artur282.OxideZap.desktop x-scheme-handler/whatsapp
flatpak run io.github.artur282.OxideZap
```

The bundle tells Flatpak to fetch `org.gnome.Platform//51` from Flathub when the runtime is not installed. To remove the application while keeping its data:

```bash
flatpak uninstall --user io.github.artur282.OxideZap
```

`flatpak uninstall` keeps user data. To remove the sandboxed settings and accounts as well:

```bash
rm -rf ~/.var/app/io.github.artur282.OxideZap
```

Inside the sandbox those files live in `~/.var/app/io.github.artur282.OxideZap/config/oxidezap/` and `~/.var/app/io.github.artur282.OxideZap/data/oxidezap/`, not in the native paths listed in [Configuration](#configuration).

The manifest is [`packaging/flatpak/io.github.artur282.OxideZap.yml`](packaging/flatpak/io.github.artur282.OxideZap.yml). It grants the network, X11/XWayland and shared IPC, the GPU device, the real `~/Downloads` directory, native notifications, and StatusNotifier. File pickers use the XDG file chooser portal. See [Packaging artifacts](#packaging-artifacts) to build it locally.

#### Flatpak sandbox limitations

- The tray needs a StatusNotifier host on the host desktop. The manifest grants the narrow `org.kde.StatusNotifierItem-2-1` name because Flatpak puts the application in a private PID namespace where OxideZap is PID 2.
- Notifications travel through `org.freedesktop.Notifications`. Notification sounds and custom hints depend on the notification daemon and portal.
- `whatsapp://` links work after Flatpak exports the desktop entry; browsers start the installed application with the URL as an argument.
- The global shortcut still uses X11 `XGrabKey` through XWayland.
- Lock on suspend and screen lock depend on the Flatpak sandbox allowing the logind system-bus signal and the desktop ScreenSaver session-bus signal. If either is filtered, those automatic locks do not fire and the desktop lock screen remains the fallback.
- The Unity LauncherEntry badge is not available inside the sandbox: OxideZap publishes `application://oxidezap.desktop`, which does not match the Flatpak desktop id. The tray badge still works.
- **Launch at login is not supported inside the sandbox.** OxideZap writes its autostart entry inside the application's private config directory, which the host session does not read. Add `flatpak run io.github.artur282.OxideZap --autostart` to the desktop's own autostart settings instead.
- The manifest does not grant audio. Voice-note playback and calls have no audio output; a custom manifest can add `--socket=pulseaudio` if needed.
- Downloads are written to the real `~/Downloads`; no other host filesystem path is exposed.

### AppImage

Each release publishes `oxidezap-vVERSION-x86_64.AppImage`. It bundles the OxideZap binary, desktop entry, icons, AppStream metadata, and license. It **does not bundle GTK 3 or WebKitGTK**: WebKitGTK ships helper processes and an injected bundle that must match the host engine, so bundling it inside an AppImage is not reliable. The AppImage is a portable application payload, not a self-contained runtime.

On top of the [system requirements](#requirements), the AppImage needs glibc 2.39 or newer (it is built on Ubuntu 24.04) and FUSE support to mount it: a `/dev/fuse` device and a setuid `fusermount`/`fusermount3` helper (`fuse3` on Ubuntu 24.04). The pinned static runtime does not need `libfuse2`. Without FUSE, use the runtime's extract-and-run flag:

```bash
chmod +x oxidezap-v0.1.0-x86_64.AppImage
./oxidezap-v0.1.0-x86_64.AppImage
# or, without FUSE:
./oxidezap-v0.1.0-x86_64.AppImage --appimage-extract-and-run
```

`AppRun` checks the dynamic linker before starting and prints the package names to install when WebKitGTK, GTK 3, or another required library is missing. To integrate the AppImage with the desktop and the `whatsapp://` handler, use AppImageLauncher or extract the desktop entry and icon with `--appimage-extract` and install them manually. There is no uninstall step: remove the AppImage file. Settings and accounts remain in the native paths under `~/.config/oxidezap/` and `~/.local/share/oxidezap/`.

## Usage

| Action | How |
| --- | --- |
| Start OxideZap | `oxidezap` |
| Reopen the first-run guide | Settings -> General -> **Show the first-run guide again** |
| Show or hide | Tray icon, or `Ctrl+Shift+W` |
| Hide or restore all windows instantly | Boss key, `Ctrl+Shift+H` by default (Settings -> General) |
| Add an account | Settings -> Accounts -> **Add account**, or the command palette |
| Switch accounts | Command palette, `Ctrl+1`..`Ctrl+9` in the native window, the tray, or Settings -> Accounts |
| Open the command palette | `Ctrl+K` in the native window, the titlebar palette button, or an optional global shortcut |
| Open settings | Titlebar gear, or tray -> **Settings** |
| Lock the app | Tray -> **Lock now**, or Settings -> Security -> **Lock now** |
| Back up or restore settings | Settings -> General -> **Export settings**, **Import settings**, or **Reset settings** |
| Change WhatsApp zoom | Settings -> Appearance -> Display size |
| Change the spell check language | Settings -> General -> Writing |
| Release memory from hidden accounts | Settings -> Performance |
| Configure a proxy | Settings -> Network |
| Test proxy connectivity | Settings -> Network -> **Test connection** |
| Preview, copy, or save diagnostics | Settings -> About -> Diagnostics |
| Quit | Tray -> **Quit**, or `oxidezap --quit` |
| Open an account | `oxidezap --account <id\|name>` |
| Open a chat | `oxidezap --chat <phone\|chat-id>` |
| Share a file to the active account | `oxidezap --share <path>`, or drag it onto an account window |
| Keep an account above other windows | Titlebar **Pin**, `Ctrl+Shift+T`, tray **Window**, or Settings -> Accounts |
| Toggle mini mode | Titlebar **Mini**, `Ctrl+Shift+M`, tray **Window**, or Settings -> Accounts |
| Toggle Do Not Disturb | `oxidezap --dnd [on\|off\|1h\|tomorrow]`, the tray menu, or Settings -> Notifications |
| Schedule quiet hours | Settings -> Notifications -> Quiet hours |
| Open the downloads folder | `oxidezap --downloads` |
| Hide all account windows | `oxidezap --hide` |
| Print status | `oxidezap --status` |
| List accounts | `oxidezap --list-accounts` |
| Open a chat link | `oxidezap "whatsapp://send?phone=..."` |

### First-run guide

A new installation opens a separate, non-modal **five-page guide** the first time OxideZap starts. The account window opens at the same time, so WhatsApp Web and its QR code are already loading behind the guide:

1. **Welcome** — OxideZap is an unofficial shell around the official WhatsApp Web, and the session stays on this computer. No telemetry is collected.
2. **Link your phone** — open WhatsApp on the phone, go to **Linked devices -> Link a device**, and point it at the QR code. If the QR stays blank, check that WebKitGTK 2.46.1 or newer is installed; older versions can hang the login. The guide never waits for the QR.
3. **Accounts and data** — each account has an isolated WebKit profile under `~/.local/share/oxidezap/accounts/<uuid>/`, settings and backups live under `~/.config/oxidezap/`, and downloads go to the configured Downloads folder.
4. **Windows and privacy** — closing to the tray, the global show/hide shortcut and boss key, native notifications and Do Not Disturb, and the app lock in Settings -> Security.
5. **Ready** — view-once messages open only on the phone and OxideZap does not bypass that, plus a reminder of where every file lives and a button to open WhatsApp Web.

```text
+----------------------------------------------------------+
|  FIRST-RUN GUIDE                              Step 2 of 5 |
|  Link your phone                                          |
|  [==========][==========][==========][          ][       ] |
|                                                          |
|  1  Open WhatsApp on your phone.                          |
|  2  Tap Linked devices, then Link a device.               |
|  3  Point the phone at the QR code in the OxideZap window.|
|                                                          |
|  [Skip]                                    [Back] [Next]  |
+----------------------------------------------------------+
```

The guide is dismissible at any time. **Skip**, **Escape**, and the window close button all record that the guide was seen, so it does not open again automatically; every page also has **Back** and **Next**, and the last page opens WhatsApp Web and returns focus to the account window. Opening Settings while the guide is visible never closes it, and locking the app or using the boss key hides it with the rest of the windows.

The guide is stored in `settings.json` as `onboarding_completed` and `onboarding_version`. Existing installations that already have a settings file are never interrupted, even though older files lack both fields. To see it again, use **Settings -> General -> Show the first-run guide again**, or reset preferences; a reset clears the guide status and opens it once more. The guide itself makes no network requests.

### Command palette

Press `Ctrl+K` while the native window owns the keyboard, or click the palette button in the account titlebar. Type to filter commands with case-insensitive substring or ordered fuzzy matching, use `Up`/`Down` to move the selection, `Enter` to run the highlighted command, and `Esc` to close without running anything. Focus returns to the account webview when the palette closes. `Tab` stays inside the palette: the search field is its single tab stop and the highlighted row is the selection.

The palette only exposes actions that already exist elsewhere in OxideZap: show or hide, show all or hide all windows, open settings, add an account, switch to account N, open the Download Center, the downloads folder, or the storage folder, lock the app, toggle Do Not Disturb, choose a theme, change zoom, retry the connection, and quit.

`Ctrl+1` through `Ctrl+9` switch to the account at that position while the native window has keyboard focus; a hidden account is shown and focused.

**Limitation.** WhatsApp Web runs in a separate WebKitGTK X11 window that owns the keyboard while it is focused, so the native in-window `Ctrl+K` and `Ctrl+1`..`Ctrl+9` only work when the GPUI native shell has focus. Settings -> General offers an optional global palette shortcut (empty by default). A global `Ctrl+K` would take precedence over WhatsApp Web's own chat search, so OxideZap does not grab it by default.

### Command line

```text
Usage: oxidezap [OPTIONS] [URL]

Actions:
  (no option)             Show the running instance, or start it
  --toggle                Show or hide the running instance
  --hide                  Hide all account windows without closing them
  --lock                  Lock the running instance
  --quit                  Quit the running instance
  --account <id|name>     Open or focus an account
  --chat <phone|id>       Open a chat in the active account (never sends)
  --share <path>          Attach a file to the active account (never sends)
  --dnd [MODE]            Set Do Not Disturb: on, off, 1h, or tomorrow
  --downloads             Open the downloads folder
  --status                Print version, accounts, DND, and window count
  --list-accounts         List accounts as id, name, and active state

Other options:
  --autostart             Start hidden when launched at login
  --smoke-test            Run the X11/WebKit overlay smoke test and exit
  -h, --help              Print this help
  -V, --version           Print the version

URLs: whatsapp://send?phone=..., https://wa.me/..., https://chat.whatsapp.com/...
```

Every action is forwarded to the running instance over the local single-instance socket and exits with status 0 on success or non-zero with a clear message when the account, chat, or shared file is invalid. When no instance is running, action commands start OxideZap and apply the action to the new instance. `--status` and `--list-accounts` are the exception: they read the settings file when the app is closed, so scripts can query them without a display.

`--status` prints stable `key: value` lines:

```text
version: 0.1.0
running: true
accounts: 2
active_account: 8a4e...
dnd: off
windows: 1
```

`--list-accounts` prints one tab-separated line per account: id, name, and `active` or `inactive`. Names are flattened to one line so a stray newline cannot forge an extra row.

`--chat` accepts a phone number (with optional `+`, spaces, dashes, dots, and parentheses) or a phone-based chat id such as `5491100000000@c.us` or `5491100000000@s.whatsapp.net`. It opens the conversation in the active account and never sends a message because it never adds WhatsApp's `text` parameter. Group ids cannot be deep-linked by WhatsApp Web and are rejected with a clear error.

`--dnd 1h` silences notifications until one hour from now. `--dnd tomorrow` silences them until the next quiet-hours end time (07:00 by default), and `--dnd on` keeps Do Not Disturb active until it is turned off. Every mode is also available from the tray menu. `--downloads` still opens the downloads folder; open the native Download Center from the tray or Settings.

### Sharing files, drag and drop, and window modes

`oxidezap --share <path>` validates the path on the calling side, resolves it to an absolute path, and forwards it over the single-instance socket. The running instance resolves the target account (the active account, or `--account <id|name>` when supplied), opens it if needed, and queues the file for WhatsApp's normal attachment preview. If OxideZap is not running, the command starts it and applies the share after the page finishes loading. A file must be a regular, non-empty file and the batch must stay within the 64 MB clipboard-paste limit; directories, missing paths, and oversized files are rejected with a message and nothing is attached. File paths are never written to the log, and no file content is evaluated as JavaScript: the existing static helper receives only base64-encoded bytes and metadata.

Drag files from a file manager onto an account window. The GTK/WebKit content area shows a **Drop to attach** overlay while a local file URI is dragged over it; dropping validates the same rules and queues the attachment. Drops on the native titlebar and exposed window margins go through the GPUI drop target and the same queue. In both cases WhatsApp keeps its own composer preview, so the user still has to press send, and unsupported drops are reported with a native notification.

Window modes are stored per account in `settings.json` as `always_on_top`, `mini_mode`, `normal_bounds`, `mini_bounds`, and `bounds_by_monitor`. **Pin** applies the EWMH `_NET_WM_STATE_ABOVE` state to the account window and keeps the WebKit overlay above it; **Mini** saves the current normal bounds and resizes the window to the compact single-pane layout. Turning mini mode off restores the saved normal bounds. The GTK overlay follows every move and resize on its next synchronization tick, including when mini mode changes the window size. Bounds are saved per monitor and clamped to an available monitor on the next open.

`--smoke-test` starts the application under X11 with a local HTML page, verifies that the GPUI window and the WebKit transient overlay have the expected geometry, prints `SMOKE OK`, and exits with status 0.

## Configuration

| Path | Purpose |
| --- | --- |
| `~/.config/oxidezap/settings.json` | Accounts, appearance (including experimental CSS), notifications, downloads, network, general, and security settings |
| `~/.config/oxidezap/backups/` | Dated settings backups written before an import or reset (`0600` files in a `0700` directory) |
| `~/.local/share/oxidezap/accounts/<uuid>/` | Per-account WebKit data (`data/`) and cache (`cache/`) |
| `~/.cache/oxidezap/` | Runtime assets such as the tray icon |
| `~/.config/autostart/oxidezap.desktop` | Autostart entry, created when launch at login is enabled |
| `$XDG_RUNTIME_DIR/oxidezap-<uid>.sock` | Single-instance socket |
| `~/Downloads` | Default download directory |
| `~/.config/oxidezap/downloads.json` | Bounded download history (newest 500 records, `0600`) |
| `~/.config/oxidezap/logs/oxidezap.log` | Application log (`0600`; rotated at 4 MiB, one previous file kept) |

OxideZap follows `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME`, and `XDG_RUNTIME_DIR` when they are set. Removing an account from Settings also removes its WebKit profile directory.

### Storage manager

Settings -> Storage reads the account directories without following symlinks. The size scan runs on a background worker with an entry and depth limit, so the values are a lower bound while WhatsApp Web is still writing; `partial` marks a scan that hit a limit or an unreadable entry.

- **Clear cache** removes `cache/` and recreates it. The WebKit data directory, and therefore the WhatsApp session, is untouched.
- **Sign out** removes `data/` and recreates it. The account and its cache stay configured, but WhatsApp Web asks for the QR code again.
- **Delete account data** removes the complete account directory and the account entry. It requires typing the account name, cannot be undone, and is refused for the last remaining account.

Every action first closes the account's `WebKitWebView` on the GTK thread, deletes the directory on a worker, and recreates the webview only when the account window is still open. The last successful cleanup timestamp is stored in `settings.json` as `last_storage_cleanup`. The **Open data folder** button opens the directory that contains every profile (`~/.local/share/oxidezap/` by default).

### Diagnostics

Settings -> About -> **Diagnostics** builds a local report that can be previewed, copied to the clipboard, or saved as a `.txt` file:

- OxideZap, GTK, and WebKitGTK versions; distribution, kernel, architecture, CPU model and thread count, and total RAM.
- Session type (X11, Wayland, or unknown), desktop environment, whether `DISPLAY` and `WAYLAND_DISPLAY` are set, the requested `GDK_BACKEND`, and the names (never the values) of `WEBKIT_*` environment variables.
- The number of configured accounts and whether an active account is selected. Account names and ids are never included.
- Settings states such as theme, zoom, notification and DND modes, tray and startup behavior, spell checking, the app lock (enabled or disabled only), the proxy mode and redacted endpoint, low-memory mode, download layout, and the last storage cleanup. Secrets are never included: no proxy username or password, no app-lock hash, and no experimental CSS.
- Runtime aggregate counts: loaded, suspended, and restoring WebKit views; connection states; crash-recovery attempts; active downloads; and whether the app is locked.
- The config, data, cache, downloads, settings, and log paths with the home directory shortened to `~`.
- The last 50 warnings and errors recorded in the current session. They are scrubbed of home paths, account UUIDs, WhatsApp ids, and secret-like assignments before they are stored.

Generation runs on a background worker, so previewing or saving the report never blocks the interface. The report contains no WhatsApp message content, cookies, session data, tokens, account names or ids, proxy credentials, or app-lock hash.

OxideZap also writes `~/.config/oxidezap/logs/oxidezap.log` with `0600` permissions and rotates it at 4 MiB, keeping the previous file as `oxidezap.log.1`. The log is more detailed than the report and may contain account ids, WhatsApp URLs, and file paths; review it before sharing. It is never attached to the diagnostics report automatically.

### Low-memory mode

Settings -> Performance controls an opt-in policy that releases the WebKit resources of hidden accounts while keeping the accounts configured and signed in. The switch is **off by default**, and the delay can be 15, 30, or 60 minutes. The setting is stored in `settings.json` as `memory.suspend_hidden`, `memory.suspend_after_minutes`, and `memory.keep_active_account`.

When the delay expires, OxideZap destroys the account's GTK window and `WebKitWebView`. That lets WebKit terminate the account's web process, which is where most of the memory goes; the `WebKitWebsiteDataManager` directories with cookies, local storage, IndexedDB, the WhatsApp login, and the cached session remain untouched. The GPUI account window stays alive and shows **Account suspended** with a **Reactivate** button. Opening the account from the tray or settings does the same thing, and shows **Restoring WhatsApp Web…** until the new view has loaded.

The policy always refuses to release:

- a visible account, including the last visible account;
- the most recently active account while **Keep the active account loaded** is on;
- an account with an active download;
- an account with an active camera, microphone, or display capture, including a muted call;
- every account while the app lock is active, because the lock screen already hides the account windows and unloading behind it would only delay the next unlock;
- every account while OxideZap is quitting.

The sweep runs at most once a minute. Hiding an account records the first hide time, so hiding it again does not postpone the delay. **Release hidden accounts now** bypasses the delay and the master switch, but not the safety list above.

Trade-offs: reactivating takes a few seconds and reloads WhatsApp Web, so the current scroll position and an unsent draft can be lost; the session, chats, and login are preserved. The released-memory figure in Settings is an estimate based on a conservative per-web-process value because WebKitGTK does not expose per-view RSS. Destroying the view does not force-kill a WebKit process; WebKit decides when to exit it.

### Performance and battery saver

Settings -> Performance also has a **Battery saver mode** with three choices:

- **Automatic** enables the saver only while the device reports battery power. OxideZap reads only `type` and `status` from `/sys/class/power_supply/*/`; if the directory is missing or unreadable, it stays in normal mode. The power source is re-read every 30 seconds, so plugging in or unplugging applies without a restart.
- **Normal** (default) never reduces animation or polling. This is what an existing `settings.json` gets when it has no `performance` field.
- **Battery saver** stays on regardless of the power source.

The setting is stored as `performance.mode` with the values `automatic`, `normal`, or `battery_saver`. Battery saver changes:

- CSS animations and transitions in WhatsApp Web are collapsed to a single near-instant frame;
- the injected connection/QR report runs every 5 s instead of every 2 s, while `online`/`offline` events still report immediately;
- the native WebKit overlay is synchronized 4 times per second instead of 10;
- the GPUI event pump and noncritical Do Not Disturb and display-scale checks run at a lower cadence;
- when low-memory mode is **already** enabled, the hidden-account delay is capped at 15 minutes and the sweep runs every 30 s instead of every 60 s.

Battery saver never turns off notifications, downloads, calls or recordings, the app lock, security checks, or the WhatsApp login, and it never enables low-memory mode by itself. Reactivating a released account, scanning the QR code, and downloading files work exactly as in normal mode.

These effects are **qualitative cadence reductions, not benchmarked FPS or watt savings**. WebKitGTK does not expose per-view CPU, GPU, or power use, so OxideZap does not claim a measured percentage improvement: WhatsApp Web and WebKit still dominate the machine's power consumption. Disabling hardware acceleration, which can make things slower, is not part of this mode.

### Backup and restore

Settings -> General includes **Export settings**, **Import settings**, and **Reset settings**. The export is a readable JSON document with a `version`, an `exported_at` UTC timestamp, and a `settings` object. An import validates the version and size, ignores unknown fields, normalizes out-of-range values such as zoom, shows a summary of the changes, and applies nothing until you confirm. Invalid or oversized files leave the current configuration untouched.

Exports contain preferences only:

- account names and per-account notification preferences, but **not** WhatsApp sessions, cookies, WebKit storage, chats, or message history;
- no unread counts or remembered window positions (runtime state);
- **no proxy credentials**. The proxy mode and host are exported, but the username and password are removed;
- **no app-lock password hash**. The lock is disabled in an export, so after importing or resetting you must set the password again in Settings -> Security.

Every import and reset writes a dated backup of the current `settings.json` to `~/.config/oxidezap/backups/` **before** changing anything, with `0600` permissions. If the backup cannot be written, the operation is cancelled. A reset restores preference defaults but keeps the configured accounts and their local sessions. It also clears the first-run guide status and opens the guide again.

### Interface language

Settings -> General -> **Interface language** offers **System** (default), **English**, and **Spanish**. System reads `LC_ALL`, `LC_MESSAGES`, `LANG`, and then `LANGUAGE`; `es_*` selects Spanish, `en_*` selects English, and unsupported or placeholder locales (`C`, `POSIX`) fall back to English. An explicit choice overrides the locale.

The value is stored as `"language"` in `settings.json` (`"system"`, `"english"`, or `"spanish"`), is included in settings exports and imports, and returns to `System` when preferences are reset. Applying a language re-renders the open native windows immediately; the tray menu and the next notifications use the new language as well.

Only the OxideZap shell is translated: titlebars and tooltips, account loading/error/retry states, the lock window, every Settings section, the tray menu, the Download Center, the command palette, the first-run guide, and native notifications. Account names, WhatsApp message content and previews, contact names, URLs, proxy values, technical log messages, CLI identifiers, and WhatsApp Web internals are deliberately left untouched. Relative dates and quiet-hours day/month names follow the interface language; file sizes keep binary units.

The implementation has no external dependency. `src/i18n.rs` maps the English source text to Spanish; a missing entry falls back to readable English instead of failing at runtime. To add a language, add a variant to `i18n::Language`, teach `Language::resolved`/`parse_locale` about its locale prefix, and add a translation table. The unit tests scan every `t!`, `tr!`, and `tn!` call in the scoped UI sources and fail when a key has no translation, when a placeholder is missing, or when a plural key is absent.

### Network and proxy

Settings -> Network controls how the WebKit data manager for each account reaches the internet:

- **System proxy** (default) delegates to the desktop proxy configuration through WebKitGTK.
- **No proxy** forces direct connections.
- **Manual proxy** takes a protocol (HTTP, HTTPS, or SOCKS), host, port, and optional username and password.

The proxy is applied when an account's `WebKitWebsiteDataManager` is created. Changing it also updates open accounts through `webkit_website_data_manager_set_network_proxy_settings`, but connections that were already established keep their route; **reopen the account** if WhatsApp Web does not pick up the new proxy. The **Test connection** button resolves the current mode and probes `web.whatsapp.com:443` on a background thread, so it never blocks the interface. HTTP and SOCKS5 proxies are tested end to end, including authentication; an HTTPS proxy is checked for reachability before its TLS handshake. Failures report the proxy response or network error.

Manual proxy credentials are stored **in plain text** inside `settings.json` because this build has no system keyring integration. The settings file is created with `0600` permissions inside a `0700` directory, credentials are never written to logs, and exports omit them. Automatic settings backups do contain them, like the app-lock password hash, and are also `0600`. If that trade-off is not acceptable, use a proxy that authenticates by network location or leave the credentials empty.

### Do Not Disturb and quiet hours

Settings -> Notifications controls the global Do Not Disturb state:

- **Off** keeps notifications enabled.
- **On** stops native notifications until you turn them off, or until a timed preset expires. The presets are **For 1 hour**, **Until tomorrow**, and **Indefinitely**.
- **Scheduled** applies a recurring **Quiet hours** window. The quiet period starts on the selected days and may cross midnight: `22:00`-`07:00` selected for weekdays covers Friday night through Saturday morning and resumes Monday night. **Every day**, **Weekdays**, and **Weekends** presets are provided, and custom days are preserved when settings are edited or imported.
- **Skip this quiet period** is a manual override. It suppresses Do Not Disturb for the current quiet period and lets the schedule resume at the next start time.

"Until tomorrow" uses the quiet-hours **end** time as its wake hour, so a `23:00`-`08:00` schedule silences until 08:00. Timed DND stores an absolute UTC instant, so it keeps counting correctly when the process restarts or the machine resumes. While DND is active, message, connection, download, crash, and error notifications are suppressed, but unread counters, window titles, the tray badge, and the tray icon's DND indicator keep updating.

Quiet hours are evaluated in the **desktop's local timezone** through libc. Times are wall-clock values, not fixed UTC offsets: a daylight-saving transition can shorten or lengthen a period by an hour, and a time inside a DST gap is resolved forward by the platform. A nonexistent gap time is not treated as an error. The app re-evaluates the state every second and after resume, so a clock change or a suspended machine cannot leave DND stuck. DND state is stored in `settings.json`; a settings export keeps the quiet-hours schedule but resets active DND state (timed countdown, indefinite manual mode, and the one-period override), which is runtime state.

## Architecture

OxideZap runs a GPUI shell and a separate GTK/WebKit thread in one process. The GPUI window is the real application window: it owns the taskbar entry, the custom titlebar, and all native chrome. A borderless GTK `Toplevel` containing the `WebKitWebView` is positioned over the GPUI content area.

```text
GPUI window (taskbar entry)
+-------------------------------------------------+
| custom titlebar (GPUI)                          |
+-------------------------------------------------+
|                                                 |
|   GTK transient overlay                         |
|   WebKitGTK + https://web.whatsapp.com          |
|                                                 |
+-------------------------------------------------+

  WM_TRANSIENT_FOR = GPUI XID
  skip_taskbar, skip_pager
  geometry synchronized every 100 ms
```

- **GPUI** owns the native shell, settings window, lock window, tray, notifications, global shortcut, and the main event loop.
- **WebKitGTK** owns the WhatsApp Web page on a dedicated thread with its own GLib loop.
- The GTK window is a **borderless transient** of the GPUI window (`WM_TRANSIENT_FOR`) with `skip_taskbar` and `skip_pager` set. It is positioned below the 42 px GPUI titlebar and resized to match the GPUI content area, with a 4 px inset so the GPUI edge-resize handles remain reachable.
- The overlay geometry is synchronized **every 100 ms** by reading the GPUI window geometry over X11 and moving or resizing the GTK window when it drifts.
- **X11 reparenting is not used.** Probes showed that GPUI child windows and reparented GTK windows do not render. The transient-overlay approach is the supported design. If the overlay cannot be positioned, OxideZap logs the exact X11 error and keeps the GTK window detached instead of crashing.
- **Accounts are isolated** with separate `WebKitWebsiteDataManager` data and cache directories.
- On a Wayland session the application runs through **XWayland**, because embedding a foreign GTK surface into a Wayland client is not portable. Native Wayland support is on the roadmap.

The two threads communicate over channels. The GPUI side sends commands such as create, show, hide, suspend, load, zoom, reload, and quit; the WebKit side reports ready state, load state, titles (from which unread counts are derived), notifications, downloads, media capture state, external links, and crashes. Media actions reuse this loop: the injected menu sends a validated `begin`/`chunk`/`end` sequence, the GTK thread writes the bytes through the download layout, and the path appears in the Download Center. When the media menu is disabled, its message handler stays registered but the injected script stops opening the menu.

Storage cleanup uses the same channel protocol. The GPUI side asks the GTK thread to close the account webview; the GTK thread then deletes the profile on a worker and reports the freed bytes back as an event. WebKitGTK 2.0.2 does not expose `WebsiteDataManager::clear`, so a coordinated in-place wipe is not available and the close/recreate cycle is the supported path. The short delay before deletion lets the web process release its SQLite and cache handles.

## Security and privacy

- OxideZap is an **unofficial client**. It is not affiliated with WhatsApp or Meta.
- The application loads the official `https://web.whatsapp.com` page. Your session, messages, and encryption keys are handled by WhatsApp Web inside WebKitGTK.
- Account data stays on your machine under `~/.local/share/oxidezap/`. OxideZap does not run a server and does not include telemetry or analytics.
- **Files and directories are private by default.** Settings are written atomically to a `0600` temporary file and renamed into place; the settings directory, account directories, and WebKit data/cache roots are `0700`. Startup applies `umask(0o077)` before WebKit creates its cookies, local storage, IndexedDB, and caches. Existing installations are migrated without following symlinks, and the app never changes permissions on the download directory.
- **Settings exports contain no secrets.** Exported files omit the app-lock password hash and every piece of WebKit session data. They also reset active DND state (timed countdown, indefinite manual mode, and one-period override) while keeping the quiet-hours schedule. Automatic backups do contain the full settings, including the hash, so they are written to a `0700` directory as `0600` files.
- **The diagnostics report is scrubbed and local.** It summarizes versions, system details, settings states, and recent warnings without chats, account names or ids, cookies, tokens, proxy credentials, the app-lock hash, or full home paths. Nothing is uploaded; the report only reaches the clipboard or a file the user chooses.
- **WebKit permissions are scoped to WhatsApp.** Only `whatsapp.com`, `wa.me`, `chat.whatsapp.com`, and WhatsApp's `whatsapp.net` utility pages may request permissions. Camera and microphone access always shows a native confirmation dialog; notifications are allowed only when the Settings notification toggle is on; every other permission type and origin is denied.
- **Media actions do not bypass WhatsApp's protections.** The menu is only reachable from an explicit right-click on media the signed-in page already decrypted and displayed. Every transfer is revalidated in Rust: the URL must be a `blob:` URL whose inner origin is WhatsApp, a media `data:` URL, or a WhatsApp host; view-once transfers are rejected; only images may be copied; and the action is size-capped. OxideZap does not intercept network traffic, decrypt media itself, extract view-once content, or add hidden download controls for restricted content. Saving still depends on WhatsApp exposing the decrypted bytes to the page, and the menu never scans chats in the background. As with any unofficial client, you remain responsible for complying with WhatsApp's terms and with the rights of the people who sent the media; the menu saves only content you can already view and does not grant access to anything else.
- The optional app lock hashes the password with **Argon2id** and verifies it locally. The lock protects access to the running UI; it does not encrypt the WebKit profile on disk.
- **Locking does not destroy state.** Account windows and the WebKitGTK view are hidden, not closed; the WhatsApp session, loaded chats, and unsent drafts remain in memory and return after unlocking. Low-memory suspension also waits while the app is locked. While locked, only the lock window is visible: account windows are unmapped and the settings window is closed.
- **Lock on suspend and screen lock use D-Bus, never shell commands.** Suspend is detected through `org.freedesktop.login1.Manager.PrepareForSleep` on the system bus. Screen lock is detected through an `ActiveChanged` signal from any interface whose name contains `ScreenSaver` (for example `org.freedesktop.ScreenSaver` or `org.gnome.ScreenSaver`) on the session bus. OxideZap never captures the screen and never reads or stores session content for these features.
- **Spell checking is local.** WebKitGTK uses the system hunspell dictionaries; OxideZap never sends composed text to a translation, cloud checking, or text service.
- Notifications show the sender and message preview. You can disable them in Settings, mute individual accounts, enable Do Not Disturb with a timed preset, or schedule recurring quiet hours. DND does not alter WhatsApp Web's own read state or silence the web page.

## Limitations

- **X11 or XWayland only.** Native Wayland is not implemented. On Wayland sessions the application relies on XWayland.
- **File-manager "Open with OxideZap" depends on the packaged desktop entry.** The application implements the receiver (`--share`, validation, single-instance forwarding, drag and drop), but the packaged desktop entries currently register only the `whatsapp://` scheme. To add **Open with**, the installed desktop entry needs `MimeType=image/*;video/*;application/pdf;` and `Exec=oxidezap --share %F`, followed by `update-desktop-database`; distribution and Flatpak packagers own that registration. Until then, drag and drop and the `--share` CLI provide the same behavior.
- **Drag and drop is an X11/XWayland feature.** The GTK drop target accepts local `text/uri-list` file URIs. Native Wayland drag sources, remote URLs, and file managers that do not publish URI lists are not handled. A dropped file still uses WhatsApp Web's own attachment input, so the composer preview remains the confirmation step.
- **Mini mode relies on WhatsApp Web's responsive layout.** The native window becomes narrow enough for WhatsApp's single-pane view, but the sidebar itself is rendered by the page, not hidden by injected CSS or JavaScript. WhatsApp Web can change its breakpoint or refuse the narrow layout; use the normal window if the composer becomes unusable.
- **Always-on-top uses the window manager's EWMH state.** Desktops that do not honor `_NET_WM_STATE_ABOVE` (or tiling compositors that override stacking) may ignore the pin.
- **Per-monitor bounds on X11 use the virtual desktop.** GPUI exposes one X11 screen for all monitors, so OxideZap persists absolute bounds and clamps them into the available desktop; the usual multi-monitor restore works because the coordinates are absolute, but a completely rearranged layout may land on the nearest visible area rather than the original monitor.
- **The global shortcuts use X11 `XGrabKey`.** They do not work on a pure Wayland session and can conflict with shortcuts registered by the desktop environment.
- **The command palette is native and separate from the web content.** WhatsApp Web runs in a transient WebKitGTK X11 window that owns the keyboard whenever it is focused. The in-window `Ctrl+K`, `Ctrl+Shift+T`, `Ctrl+Shift+M`, and `Ctrl+1`..`Ctrl+9` shortcuts therefore only reach GPUI when the native window has focus. The optional global palette shortcut uses the same X11 grab and is empty by default because grabbing `Ctrl+K` system-wide would take precedence over WhatsApp Web's own chat search. The palette restores focus to the account webview after it closes.
- **Lock on suspend and screen lock depend on the desktop exposing its D-Bus signals.** GNOME, KDE, Cinnamon, MATE, and XFCE normally publish a ScreenSaver service, but some Wayland compositors, minimal window managers, and sandboxed packages do not. Lock on suspend needs logind on the system bus; Flatpak builds may not be allowed to receive that signal. If the signals are unavailable, OxideZap logs the reason and manual, lock-on-hide, and idle locking still work; the desktop's own lock screen remains the fallback. This limitation is inherent to the desktop and is not masked by claiming support.
- **WhatsApp Web dominates memory use.** OxideZap keeps the native shell small, not the web content. The opt-in low-memory mode can release hidden accounts, but the released-memory figure is an estimate; WebKit decides when the web process actually exits.
- **Low-memory reactivation is not instant and can lose transient state.** Showing a suspended account recreates its WebKit view and reloads WhatsApp Web, which takes a few seconds. The login, chats, and settings survive, but the current scroll position and an unsent draft may not. Suspension is opt-in and is refused while a download, call, or recording is active.
- **Battery saver is not a benchmark.** It reduces animation, polling, and synchronization work, but OxideZap cannot measure per-view CPU, GPU, or power use and does not claim a percentage saving. Automatic mode depends on `/sys/class/power_supply`; a missing, unreadable, or unusual power supply falls back to normal mode. Battery saver cannot make WhatsApp Web or WebKit use less power than the page itself demands.
- **Calls and voice notes** depend on WebKitGTK's WebRTC support and on the desktop portal configuration. They are not guaranteed on every distribution.
- **Notifications** require a notification daemon. **The tray** requires a StatusNotifier host; desktops without one will not show the tray icon. Notification sounds depend on the daemon and sound theme, and some hosts ignore dynamic tray pixmaps, in which case the per-account tooltip remains available. Quiet hours follow the system timezone and the desktop's DST rules; a time that falls in a DST gap is normalized forward by the platform.
- **Spell checking depends on system dictionaries and WhatsApp Web.** WebKitGTK checks only languages whose hunspell dictionaries are installed, and WhatsApp Web owns the composer, so it can override spellcheck attributes or Enter-to-send behavior. Language changes apply to open accounts immediately, but the composer may need focus or a reload before it shows new underlines.
- **Storage sizes are approximate.** The scanner skips symlinks and stops after 200,000 entries or 32 directory levels. WebKit may create, grow, or release files while a scan is running, so the reported total is a lower bound; a `partial` marker means the scan hit a limit or an unreadable entry. Clearing data closes and recreates the account webview, so an open chat reloads after the operation.
- **Destructive storage actions are guarded.** Signing out and deleting account data require confirmation, deleting account data requires typing the account name, and the last account cannot be deleted. The webview is always closed before its profile is touched; OxideZap never deletes files outside `accounts/` and refuses symlinked account or profile directories.
- **Experimental CSS can break WhatsApp Web.** The selectors belong to WhatsApp, not to OxideZap, and change without notice. Hiding a container can break the QR login, calls, media, or the composer, and a WhatsApp update can leave a loaded stylesheet partially applied. There is no compatibility guarantee and no per-selector fallback; disable the toggle or use **Reset** if the interface breaks. The CSS is CSS-only: it never executes JavaScript, never changes message data, and is not a way to automate WhatsApp.
- **Connection detection is best-effort.** The injected script relies on `navigator.onLine` and a small set of QR-container selectors. `navigator.onLine` reflects the OS network interface, not real reachability, and WhatsApp Web can change its DOM at any time. Native load-failure signals and the 30 s load timeout are the fallback; WhatsApp Web may also reconnect on its own before a reload is needed.
- **Automatic reconnect does not preserve an unsent draft.** OxideZap only reloads a page that is already in a load error state, but a draft composed inside a page that later errors may be lost. Use **Retry now** deliberately when the page is still usable.
- **The first login requires a QR scan** from the WhatsApp mobile app.
- **Downloads depend on WebKit's delivery mode.** Files served as ordinary HTTP(S) responses can be tracked, cancelled, and retried. `blob:` and `data:` media saved through the media action menu also appear in the Download Center, but they cannot be retried later because the page revokes those URLs. Encrypted media that never becomes a normal download and view-once media do not appear in the Download Center; view-once downloads are deliberately unsupported.
- **The media action menu is experimental and DOM-dependent.** It recognizes media from the element under the pointer and a bounded set of view-once, sticker, and avatar markers. WhatsApp Web can change its markup at any time; the menu then stops appearing for the affected media instead of breaking the chat. It is enabled by default, can be turned off in Settings > General, and only handles media the page already decrypted for the signed-in account. A single transfer is capped at 64 MB; larger images, videos, or audio must use WhatsApp's own download control. Document attachments keep working through WhatsApp's own download button, which also feeds the Download Center. Bulk download of a selection is not implemented: WhatsApp Web forwards or deletes selected messages but does not expose their media as file URLs, so every media action remains an explicit per-item gesture.
- **No auto-update.** Reinstall or replace the binary to update.
- **Diagnostics are best effort.** GTK and WebKitGTK versions come from the loaded libraries, and CPU, memory, distribution, and session details come from `/proc`, `/etc/os-release`, and environment variables; a sandbox may hide some of them. The session log is written at info level and can contain account ids, WhatsApp URLs, and file paths, so review it before sharing. It is never included in the report automatically.
- **The app lock does not encrypt data at rest.** It gates the UI, not the files on disk.
- **Proxy changes may need an account reopen.** WebKitGTK applies the new proxy configuration to the account's data manager, but an already-established HTTP/2 or WebSocket connection may keep the previous route until the account is reopened. Manual proxy credentials are stored as plain text in `settings.json`; see [Network and proxy](#network-and-proxy).
- **The proxy test does not validate an HTTPS proxy's TLS handshake.** It verifies that the proxy host and port are reachable; opening an account exercises the full TLS proxy path.
- **The AppImage is not self-contained.** It bundles OxideZap but requires the host's GTK 3 and WebKitGTK 4.1 >= 2.46.1; see [AppImage](#appimage).
- **Flatpak sandboxing changes desktop integration.** Tray, notifications, autostart, and audio are subject to the limitations documented under [Flatpak](#flatpak).
- **Screen readers cannot inspect the native GPUI shell.** The native titlebar, settings window, and lock screen are keyboard-operable: `Tab` and `Shift+Tab` move focus, focused buttons activate with `Enter` or `Space`, and focus is drawn with a visible border. GPUI 0.2.2 has no AccessKit/AT-SPI bridge, so assistive technology has no semantic view of these native controls; the WhatsApp Web content inside WebKitGTK is unaffected.
- **Linux only.** There are no macOS or Windows builds.

## Development

Before opening a pull request, run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Run the X11/WebKit integration smoke test on a real X11 display:

```bash
DISPLAY=:0 cargo run -- --smoke-test
```

The smoke test prints `SMOKE OK` and exits with status 0 when the GPUI window and the WebKit transient overlay are positioned correctly. It needs a Vulkan-capable display; Xvfb does not provide the DRI3 support GPUI needs for presentation. CI runs formatting, Clippy, unit tests, and a release build on Ubuntu 24.04; the smoke test is a local check.

### Packaging artifacts

Both package scripts write into `dist/`:

```bash
# Flatpak: needs flatpak-builder and the GNOME 51 SDK from Flathub.
flatpak remote-add --user --if-not-exists flathub \
  https://flathub.org/repo/flathub.flatpakrepo
flatpak install --user flathub org.gnome.Sdk//51 org.gnome.Platform//51
packaging/flatpak/build-flatpak.sh
# Or let flatpak-builder install the runtime and Rust SDK extension:
INSTALL_DEPS_FROM_FLATHUB=1 packaging/flatpak/build-flatpak.sh

# AppImage: builds the release binary and downloads pinned appimagetool 1.9.1.
packaging/appimage/build-appimage.sh
```

The Flatpak manifest builds with Cargo network access inside the build sandbox, and `Cargo.lock` pins every crate. That keeps the repository small, but a Flathub submission should instead vendor the crates with [`flatpak-cargo-generator`](https://github.com/flatpak/flatpak-builder-tools/tree/master/cargo) and remove the `--share=network` build argument. The AppImage script verifies the appimagetool SHA-256 before using it and sets `SOURCE_DATE_EPOCH` from the last commit when it is not already set.

CI validates the Flatpak manifest, desktop entries, AppStream metadata, and shell scripts on every push. The release workflow builds the tarball, Flatpak, and AppImage on `v*` tags and publishes all three assets on clean Ubuntu 24.04 runners. A release should still be smoke-tested on a clean desktop before it is announced, because the Flatpak sandbox changes tray, notification, and audio behavior.

## Roadmap

- [x] v0.1.0: GPUI shell, WebKitGTK overlay, multi-account support, tray, notifications, app lock, settings, CLI, installer, and CI
- [x] Distribution packaging: Flatpak and AppImage artifacts
- [ ] Native Wayland support without XWayland
- [ ] Flathub publication and aarch64 packaging artifacts
- [ ] Broader desktop integration

## Contributing

Issues and pull requests are welcome. For larger changes, open an issue first so the approach can be discussed. Please run the formatting, Clippy, and test commands above before submitting a pull request.

## License

MIT. See [LICENSE](LICENSE).
