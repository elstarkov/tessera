---
name: verify
description: Build, launch, and drive Tessera end-to-end on macOS to verify UI behaviour (dialogs, shortcuts, pane/tab flows) with screenshot evidence.
---

# Verifying Tessera UI changes

Build and launch:

```sh
cargo build --release
target/release/tessera &          # process name is `tessera` (lowercase)
```

Careful: a bundled copy may already be running as `Tessera` (capital T,
from dist/Tessera.app) — possibly hosting the very session you are in.
`pgrep -x tessera` matches only the dev binary; never send input to the
bundled process.

## Driving the UI without Accessibility/TCC grants

`osascript`/System Events keystrokes and `lldb -p` both fail without
per-app TCC permissions. What does work: inject a driver dylib via
`DYLD_INSERT_LIBRARIES` (the cargo binary is not hardened) that reads
commands from a FIFO and posts real `NSEvent`s / calls AppKit on the
main thread inside the app. Commands worth supporting: `raise`, `winid`,
`close` (`performClose:` = red-button/Cmd+Q path), `key <keyCode>
<chars|RET|ESC> [cmd,shift]`, `ping`.

```sh
clang -dynamiclib -fobjc-arc -framework AppKit -o driver.dylib driver.m
mkfifo driver.fifo
DYLD_INSERT_LIBRARIES=$PWD/driver.dylib \
  TESSERA_DRIVER_FIFO=$PWD/driver.fifo TESSERA_DRIVER_OUT=$PWD/driver.out \
  target/release/tessera &
echo "key 12 q cmd" > driver.fifo     # Cmd+Q; q=12 w=13 d=2 t=17 RET=36 ESC=53
```

Posted key events go through `[NSApp sendEvent:]`, so menu key
equivalents (Cmd+Q) take the real path. `raise` first so the window is
key. Screenshots: get the id with `winid`, then
`screencapture -x -o -l <windowid> out.png` (captures occluded windows;
needs Screen Recording permission, which is typically granted).

Useful flows: Cmd+D split, Cmd+T tab, Cmd+W close pane (last pane close
exits the app), Cmd+Q quit confirmation, `exit`/Ctrl-D in a shell.
Verify liveness with `pgrep -x tessera`; a clean quit exits with code 0.
