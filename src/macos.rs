//! macOS-specific glue.

use objc2::runtime::AnyObject;
use objc2::sel;
use objc2_app_kit::{NSApplication, NSView};
use objc2_foundation::MainThreadMarker;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// Reroute the app menu's Quit item (the Cmd+Q key equivalent) from
/// `terminate:` - which kills the process before the frame loop can react -
/// to `performClose:` on the main window. Quitting then arrives as a regular
/// window-close request, which `Tessera::update` holds for confirmation.
pub fn route_quit_through_close(cc: &eframe::CreationContext<'_>) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Ok(handle) = cc.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return;
    };
    // SAFETY: an AppKit window handle carries a live NSView pointer, and the
    // marker above proves we're on the main thread.
    let view = unsafe { appkit.ns_view.cast::<NSView>().as_ref() };
    let Some(window) = view.window() else {
        return;
    };

    // winit builds the menu bar before eframe creates the window, so the
    // Quit item exists by the time we run. Find it by its action rather than
    // by position, in case the menu layout changes.
    let app = NSApplication::sharedApplication(mtm);
    let Some(menubar) = (unsafe { app.mainMenu() }) else {
        return;
    };
    for item in unsafe { menubar.itemArray() }.iter() {
        let Some(submenu) = (unsafe { item.submenu() }) else {
            continue;
        };
        for entry in unsafe { submenu.itemArray() }.iter() {
            if unsafe { entry.action() } == Some(sel!(terminate:)) {
                let target: &AnyObject = &window;
                // SAFETY: the menu item outlives neither NSApp nor the main
                // window it now targets; both live for the whole process.
                unsafe {
                    entry.setTarget(Some(target));
                    entry.setAction(Some(sel!(performClose:)));
                }
                return;
            }
        }
    }
}
