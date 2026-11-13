# GPUI Fast adoption

OxideZap uses [`gpui-fast`](https://github.com/longbridge/gpui-fast) 0.1.15 and
`gpui-fast-platform` 0.1.15 instead of upstream `gpui` 0.2.2. The fork provides
retained view/layout reuse and the window-composition API used by the WebKit
overlay.

## Version and update policy

- `gpui-fast`, `gpui-fast-platform`, and the platform crates are pinned to
  `0.1.15` in `Cargo.toml`. They are published together and must be upgraded
  together.
- `libc` is no longer pinned to `0.2.180`: the fork's wgpu dependency requires
  `libc >= 0.2.183`.
- Treat a fork upgrade like a platform upgrade: run the full test suite, the
  real-display smoke test, and the A/B check below before updating the pin.
- The fork follows upstream GPUI snapshots and does not guarantee upstream
  semver compatibility. Review its release notes and diff the platform API
  before upgrading.

## Retained mode

The native shell already keeps state in `Entity` values. The migration changed
the application constructor to `gpui_platform::application()` and adapted the
small API differences (`Window::focus*` now take `&mut App`, `Timer` comes from
`smol`, `WindowOptions` has additional fields, and `TextSystem::paint` takes
alignment arguments).

Retained mode is enabled by default. `GPUI_VIEW_RETENTION=0` remains a
diagnostic escape hatch; it was verified to start and render the same shell.

## A/B measurement

Method: fresh isolated XDG directories, 7 s startup settle, then 10 s of idle
CPU sampled from `/proc/<pid>/stat` (user + system ticks divided by
`CLK_TCK`) on the same X11 desktop. WebKit/WhatsApp Web dominates both runs, so
this is a shell-cost check, not a WhatsApp benchmark.

| Build | CPU over 10 s | RSS |
| --- | ---: | ---: |
| upstream `gpui 0.2.2` | 0.680 s | 216 MB |
| `gpui-fast` retained | 0.120 s | 214 MB |
| `gpui-fast` with `GPUI_VIEW_RETENTION=0` | 0.130 s | 213 MB |

The difference is within run-to-run noise for a single sample, but the fork is
not slower in this shell. A proper benchmark should repeat the sample and open
Settings and an active chat; those numbers are not claimed here.

## Window composition (experimental)

The composition code remains available behind `OXIDEZAP_COMPOSITION=1`, but it
is **not enabled by default**. The current GTK/WebKit embedding path creates an
X11 container and hands it to `GtkWindow`; on this X11/GTK stack GTK aborts
with a `gtk_widget_get_frame_clock` assertion during startup. The default path
therefore stays on the proven `WM_TRANSIENT_FOR` overlay with its 4 px inset
and periodic geometry sync.

The composition path is tracked by #38 and needs a different GTK embedding
approach (or a WebKitGTK child-window API) before it can become the default.
Do not enable it in production. The smoke test covers the default overlay; the
composition smoke branch exists only for future work.

## Rollback

To return to upstream GPUI:

1. Restore `gpui = "0.2.2"` and remove `gpui_platform`, `raw-window-handle`,
   `rand_core`, and `smol` if unused.
2. Restore the `Application::new()` constructor and the pre-fork API calls
   (`Window::focus*`, `Timer`, `WindowOptions`, `TextSystem::paint`).
3. Revert the composition fields and `GtkCommand::Create.composition_xid` /
   `SetEmbeddedBounds`; the transient overlay path is already retained.
4. Restore the `libc = "=0.2.180"` pin if the upstream dependency tree needs it.
