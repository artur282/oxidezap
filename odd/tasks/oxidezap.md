# OxideZap v0.1.0

Objective: a Linux WhatsApp Web desktop client with a native GPUI shell and WebKitGTK content.
Branch: `main` (repository just created; the user owns further commits).
Delivery: single public repository; release packaging and CI in the same feature.
Test runner: `cargo test`; integration smoke test on a real X11 display with `--smoke-test`.

## Specs

S1. **Repository**: public GitHub repository `artur282/oxidezap`, with a README, MIT license, description, and topics. Status: done.

S2. **Native shell**: a GPUI window with a custom titlebar (app icon, account name, settings, minimize/maximize/close) and a content area occupied by the WhatsApp Web view. The window must use client-side decorations and remember its size/position.

S3. **Web content**: the official `https://web.whatsapp.com` page runs in WebKitGTK 4.1 on a dedicated GTK thread. The GTK window is a borderless, skip-taskbar transient of the GPUI window, positioned and synchronized over the GPUI content area (X11/XWayland). Reparenting into the GPUI window is not used: probes proved that GPUI child windows and reparented GTK windows do not render. If the overlay cannot be positioned, log the exact X11 error and keep the GTK window detached rather than crashing.

S4. **Persistent login**: WebKit data and caches live under `~/.local/share/oxidezap/accounts/<id>/`; a login survives restarts. The first account is created automatically.

S5. **Multiple accounts**: accounts can be added, renamed, and removed. Every account has its own `WebKitWebsiteDataManager`, its own GPUI window, and its own unread count. The tray lists accounts.

S6. **Desktop integration**: system tray (StatusNotifierItem) with show/hide, accounts, settings, lock, quit; close-to-tray; unread count in the tray tooltip and, on supported desktops, the Unity launcher badge; native notifications with title/body; single instance; `Ctrl+Shift+W` global show/hide; launch at login.

S7. **Settings**: a native GPUI settings window with Accounts, Appearance (theme, zoom 80–150%), Notifications, General (close to tray, start at login, remember bounds), Security (lock), and About. Changes persist to `~/.config/oxidezap/settings.json` atomically.

S8. **App lock**: optional Argon2id password. Lock on launch, on hide, and after an idle timeout. A lock window covers all account windows until the password is verified. Removing the password disables the lock and keeps the existing accounts and sessions; this is a deliberate v0.1.0 deviation from whatRust's account-clearing reset.

S9. **Links and downloads**: external `http(s)`, `mailto:`, and `tel:` links open in the system browser/app; `wa.me`, `chat.whatsapp.com`, and `web.whatsapp.com` stay in the app. Internal `whatsapp.net` utility pages (for example `flows.whatsapp.net/flows/cache_management/`) stay in the webview and never open a browser tab. Downloads go to `~/Downloads` and produce a native notification with the destination. `whatsapp://` arguments open the matching chat.

S10. **Quality**: unit tests for settings serialization, unread parsing, shortcut parsing, account path isolation, and lock hashing. A `--smoke-test` mode starts the app on a real X11 display with a local HTML page, verifies the embedded webview and transient geometry, prints `SMOKE OK`, and exits 0. Xvfb cannot present GPUI because it lacks DRI3, so the smoke test is local-only. CI runs formatting, Clippy, tests, and a release build on Ubuntu 24.04.

S11. **Packaging**: `install.sh` installs the binary to `~/.local/bin`, the `.desktop` entry, the icon, and the `whatsapp://` handler. A GitHub Actions workflow builds release artifacts and runs tests. README documents requirements, build, install, usage, limitations, and the unofficial status.

## Tasks

- T1 Repository scaffolding, README, license, topics. Route: inline. Status: done.
- T2 Core skeleton: Cargo manifest, paths, settings model, theme, app entry point, single instance. Route: inline. Status: done.
- T3 WebKitGTK thread, data isolation, transient overlay, notifications/downloads/links. Route: inline (critical path). Status: done.
- T4 GPUI account window, titlebar, account switcher. Route: inline. Status: done.
- T5 Settings window, lock window, shared UI components. Route: delegated (independent UI unit) after T2 interfaces. Status: done.
- T6 Tray, badge, native notifications plumbing, autostart, global shortcut. Route: delegated (independent integration unit) after T2 interfaces. Status: done.
- T7 Packaging, CI, install script, README final pass, icon PNGs. Route: delegated (independent delivery unit). Status: done.
- T8 Tests, smoke test, local verification, screenshots. Route: inline + delegated verifier if a high-risk item is named. Status: done.

## Log

L1 (2026-11-04, verbatim user request):

> https://github.com/karem505/whatRust
> mira como funciona whatRust y crea un WhatsApp Web Desktop Client pero usando gpui en lugar que tauri
>
> el cliente se llamara OxideZap sera para linux
>
> empieza creando el repositorio con gh cli(respsitorio con su readme tags descripcion y todas sus caracteristecas y que sea publico)
>
> puede usar varios agentes para hacer todo esto

L2. Studied whatRust (Tauri v2): wraps `web.whatsapp.com`, features include multiple isolated accounts, tray/badge, notifications, lock (Argon2id + biometric), zoom, downloads, links, `whatsapp://`, autostart, global shortcut, single instance, remembered bounds. OxideZap keeps the same product surface but replaces Tauri with GPUI for the native shell and embeds WebKitGTK directly.

L3. Architecture decision (revised after probes): GPUI has no webview. Use GPUI for the native shell and a dedicated GTK/WebKit thread for the page. GPUI child windows (any depth) are not displayed, and reparenting a GTK window into the GPUI window renders blank, so embedding by X11 reparenting is not viable. The working design is a separate borderless GTK `Toplevel` with `WM_TRANSIENT_FOR` set to the GPUI XID, `skip_taskbar`/`skip_pager`, positioned over the GPUI content area and synchronized every 100 ms. The GPUI window is the taskbar window and draws the custom titlebar; the GTK transient draws the WhatsApp Web content. Force the GPUI X11 backend and run under XWayland on Wayland sessions. Stock GPUI 0.2.2 is used (no registry patch).

L4. Build environment: GPUI 0.2.2 compiles with `libc = "=0.2.180"` (the `xattr 0.2.3` transitive dependency does not compile against libc 0.2.191). WebKitGTK 2.52.6 runtime is installed; development headers are provided through a local extracted sysroot during development. CI installs the distribution packages.

L5. Verification evidence (2026-11-06): `cargo fmt --all -- --check` clean; `cargo clippy --all-targets -- -D warnings` clean; `cargo test` -> 35 passed, 0 failed; `cargo build --release` succeeds. Real-display smoke test: `SMOKE OK: GPUI 1180x780+50+50, WebKit 1172x734+54+92, transient=true`, exit 0. The full app loads WhatsApp Web with the QR/login screen, the custom GPUI titlebar, and the settings window.

L6. Runtime fixes found during verification: window creation is deferred with `cx.defer` because opening a window inside `model.update` panicked; the GTK overlay must be `set_resizable(true)` or X11 configure requests do not resize it; `ksni` uses `async-io` instead of Tokio to avoid a zbus reactor panic; `account_xids` originally stored the GTK XID and overwrote the GPUI XID, so hide/show did not unmap the GPUI window (fixed; `--toggle` now hides both, `--show` restores both, and `Ctrl+Shift+W` works); the lock screen's Enter handler read the `TextField` while it was being updated, causing a GPUI reentrancy panic (fixed by deferring the callback); the app icon uses a monochrome SVG because GPUI's `svg()` painter is monochrome and only paints when `text_color` is set; the WebKit overlay is inset by 4 logical px so the GPUI edge-resize handles remain reachable (verified by dragging the right edge from 1180 to 1232 px).

L7. UI verification: Appearance theme switches System/Light/Dark; the settings window renders Accounts, Appearance, Notifications, General, Security, and About; setting an Argon2id password writes the hash; the lock window renders and rejects a wrong password with "Incorrect password."; `--lock`, `--toggle`, `--show`, and the global shortcut were exercised. The test session used isolated XDG directories under `/tmp/opencode/oz-test`; repository files contain no runtime data.

L8. Packaging: README, `install.sh`, `.desktop`, AppStream metadata, and GitHub Actions build/release workflows were added and validated with `bash -n`, XML/YAML parsing, `desktop-file-validate`, `appstreamcli validate --no-net`, and an isolated install/uninstall. The Xvfb smoke job was removed because GitHub-hosted Xvfb lacks DRI3/Vulkan presentation; the smoke test is documented as a real-display local check. No commit was made: the user's global rule reserves commits and pushes for the user.

L9 (2026-11-06, user report verbatim):

> cada vez que se abre la secion de ws se abre una pesataña en mi navegador con esta url https://flows.whatsapp.net/flows/cache_management/ y dice esto 0 WA Flows expired cached states removed.

Diagnosis: `flows.whatsapp.net` is not a `whatsapp.com` host, so the new-window router classified it as an external link and called the system browser. Fix: `links::is_internal_url` recognizes `whatsapp.net` and its subdomains; new-window requests for those hosts are ignored, and navigations are allowed to stay inside the WebKit view. Added a regression test for the flows URL, `mmg.whatsapp.net`, and lookalike hosts. Runtime debug log after the fix: `allowed an internal WhatsApp navigation uri=https://flows.whatsapp.net/flows/cache_management/`; no new external-open event is emitted.
