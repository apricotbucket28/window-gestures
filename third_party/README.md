# Reactor in Window Gestures

[windows-reactor](windows-reactor/) is a snapshot of `crates/libs/reactor` from [microsoft/windows-rs](https://github.com/microsoft/windows-rs). [windows-rs-revision.txt](reactor-popup/windows-rs-revision.txt) identifies the upstream commit. [upstream.patch](reactor-popup/upstream.patch) records the local changes already applied to the snapshot.

## What reactor-popup does

[reactor-popup/screen.rs](reactor-popup/screen.rs) extends Reactor’s private `TransientMenuHost`. It runs one Reactor UI thread and shares one transparent XAML Island between the tray and window-action menus. It builds both `MenuFlyout` layouts at startup and reuses their controls. [The tray controller](../src/platform/tray.rs) supplies labels and checked states, handles selections, and releases the window-action reservation on dismissal.

Both menus use WinUI’s `MenuFlyoutItemThemePaddingNarrow` resource. A tray click or window gesture can open the flyout before its island has recorded any mouse or keyboard input. Setting the padding when constructing the items keeps the first open consistent with later opens. [WinUI selects the default padding from the input recorded by the island.](https://github.com/microsoft/microsoft-ui-xaml/blob/main/dxaml/xcp/dxaml/lib/MenuFlyoutItemBase_Partial.cpp#L34-L57)

When showing a menu, the extension positions the anchor at the requested physical screen point and asks Windows for foreground activation. It calls `ShowAt` only after the island’s input window or one of its children owns keyboard focus. A short dispatcher timer retries a delayed handoff and stops if the user switches to another window. Keep that order when replacing the host, since [WinUI uses native focus loss to dismiss windowed popups](https://github.com/microsoft/microsoft-ui-xaml/blob/main/docs/design-notes/focus.md#windowed-popups).

## What the upstream patch changes

| Upstream file | Reason for the local change |
| --- | --- |
| `Cargo.toml` | Replaces workspace dependencies with Git dependencies at the same upstream revision so the snapshot builds outside the windows-rs workspace. The update script expands `@WINDOWS_RS_REVISION@`. |
| `src/lib.rs` and `src/native/mod.rs` | Export `ScreenMenus`, `ScreenMenuItem`, and `ScreenMenuChange` to Window Gestures. |
| `src/native/transient_menu.rs` | Includes the local extension and lets it wait for the anchor’s `Loaded` event. It follows the Windows app color mode from startup and shows the already-transparent host without activation, including when Window Gestures starts hidden. |
| `src/native/bindings.rs` | Exposes `IDesktopSiteBridge::WindowId` so the extension can focus the island’s Win32 input window. If the bindings change, check the interface and vtable order against the Windows App SDK metadata. |
| `src/native/app/mod.rs` | Gives the extension access to Reactor’s runtime bootstrap and UI thread lifecycle. It also calls the window setup callback after initial sizing and before activation. |
| `src/window.rs` | Adds `WindowPolicy::before_show`. [Settings and Quick Guide](../src/platform/ui_window.rs) use it to center the native window and set the embedded title-bar icon before it appears. |
| `src/declaration.rs` and `src/native/winui/objects.rs` | Expose Escape as a keyboard accelerator so Settings and Quick Guide can close when the focused control leaves Escape unhandled. |

## Updating and resolving conflicts

Follow the normal [update procedure](../BUILDING.md#update-reactor) first. If the patch fails to apply, the script leaves the current snapshot and revision pin in place.

1. Open a clean, disposable windows-rs checkout at the new commit. Expand the patch’s revision placeholder and apply it with rejects from the Window Gestures root. Set `$upstream` to the disposable checkout.

   ```powershell
   $upstream = 'C:/path/to/windows-rs'
   $revision = git -C $upstream rev-parse HEAD
   $patch = 'third_party/reactor-popup/upstream.patch'
   $expanded = [System.IO.Path]::GetTempFileName()
   (Get-Content -LiteralPath $patch -Raw).Replace('@WINDOWS_RS_REVISION@', $revision) |
       Set-Content -LiteralPath $expanded -Encoding utf8 -NoNewline
   git -C $upstream apply --reject --directory=crates/libs/reactor $expanded
   ```

2. Resolve each `.rej` in the disposable checkout using the intent above. Keep popup behavior in [screen.rs](reactor-popup/screen.rs). If upstream now provides an equivalent API, adapt the callers and drop the corresponding hunk once it preserves the focus, dismissal, or window-placement behavior.
3. Regenerate the crate-relative patch from the resolved diff, replacing the concrete dependency revision with the placeholder again.

   ```powershell
   git -C $upstream diff --relative=crates/libs/reactor HEAD -- crates/libs/reactor |
       Where-Object { $_ -notmatch '^index ' } |
       ForEach-Object { $_.Replace($revision, '@WINDOWS_RS_REVISION@') } |
       Set-Content -LiteralPath $patch -Encoding utf8
   ./scripts/update-reactor.ps1 -Revision $revision -Upstream $upstream
   cargo check --all-targets
   ./scripts/update-reactor.ps1 -Check -Upstream $upstream
   cargo test --locked --all-targets
   ```

4. Check the first **Win+middle-click** after restarting Window Gestures, before moving or resizing any window. Confirm the menu takes focus, stays open, accepts keyboard input, and dismisses with Escape and an outside click. Set Windows and app color modes to different values and confirm both menus follow the app mode. Confirm Settings and Quick Guide open centered with their title-bar icons. Review the revision pin, patch, refreshed snapshot, and `Cargo.lock` together.
