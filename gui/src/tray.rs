//! Tray mode: an icon in the macOS menu bar, and minimizing hides the window
//! there instead of in the Dock. NSH 0.4 has no status item, so this talks
//! to AppKit directly. Calls come from the engine thread (menu handlers) and
//! run on the main thread, where AppKit lives.
//!
//! On other systems tray mode is not offered and minimize is the window's.

#[cfg(target_os = "macos")]
mod imp {
    use std::cell::RefCell;
    use std::ptr::NonNull;

    use block2::RcBlock;
    use dispatch2::DispatchQueue;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
    use objc2::{define_class, msg_send, sel, AllocAnyThread, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSImage, NSSquareStatusItemLength, NSStatusBar,
        NSStatusItem, NSWindow, NSWindowButton, NSWindowDidBecomeKeyNotification,
        NSWindowDidMiniaturizeNotification,
    };
    use objc2_foundation::{NSData, NSNotification, NSNotificationCenter, NSObjectProtocol, NSSize, NSString};

    static LOGO: &[u8] = include_bytes!("../../assets/logo.png");

    define_class!(
        /// Receives the status item's and the minimize button's clicks.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "PiFamulusTrayTarget"]
        struct Target;

        impl Target {
            #[unsafe(method(toggle:))]
            fn toggle(&self, _sender: Option<&AnyObject>) {
                let mtm = self.mtm();
                if is_hidden() {
                    show(mtm);
                } else {
                    hide(mtm);
                }
            }

            #[unsafe(method(hideToTray:))]
            fn hide_to_tray(&self, _sender: Option<&AnyObject>) {
                hide(self.mtm());
            }
        }
    );

    struct Tray {
        item: Retained<NSStatusItem>,
        target: Retained<Target>,
        observers: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    }

    thread_local! {
        static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
        /// The window while it is hidden in the tray.
        static HIDDEN: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
    }

    fn on_main(work: impl FnOnce(MainThreadMarker) + Send + 'static) {
        DispatchQueue::main().exec_async(move || work(MainThreadMarker::new().expect("main queue")));
    }

    fn is_hidden() -> bool {
        HIDDEN.with(|h| h.borrow().is_some())
    }

    /// The app's one window.
    fn window(mtm: MainThreadMarker) -> Option<Retained<NSWindow>> {
        if let Some(w) = HIDDEN.with(|h| h.borrow().clone()) {
            return Some(w);
        }
        let app = NSApplication::sharedApplication(mtm);
        app.mainWindow().or_else(|| app.windows().iter().find(|w| w.canBecomeMainWindow()))
    }

    fn hide(mtm: MainThreadMarker) {
        let Some(w) = window(mtm) else { return };
        if TRAY.with(|t| t.borrow().is_none()) {
            w.miniaturize(None);
            return;
        }
        w.orderOut(None);
        HIDDEN.with(|h| *h.borrow_mut() = Some(w));
        // No Dock icon while only the tray icon shows.
        NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    }

    #[allow(deprecated)]
    fn show(mtm: MainThreadMarker) {
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        let w = HIDDEN.with(|h| h.borrow_mut().take()).or_else(|| window(mtm));
        if let Some(w) = w {
            if w.isMiniaturized() {
                w.deminiaturize(None);
            }
            w.makeKeyAndOrderFront(None);
        }
        app.activateIgnoringOtherApps(true);
    }

    fn install(mtm: MainThreadMarker) {
        if TRAY.with(|t| t.borrow().is_some()) {
            return;
        }
        let target: Retained<Target> = unsafe { msg_send![Target::alloc(mtm), init] };
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSSquareStatusItemLength);
        if let Some(button) = item.button(mtm) {
            if let Some(image) = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(LOGO)) {
                image.setSize(NSSize::new(18.0, 18.0));
                button.setImage(Some(&image));
            }
            button.setToolTip(Some(&NSString::from_str("pi-famulus")));
            // SAFETY: the target is kept in TRAY for as long as the item.
            unsafe {
                button.setTarget(Some(&target));
                button.setAction(Some(sel!(toggle:)));
            }
        }
        if let Some(b) = window(mtm).and_then(|w| w.standardWindowButton(NSWindowButton::MiniaturizeButton)) {
            // SAFETY: as above.
            unsafe {
                b.setTarget(Some(&target));
                b.setAction(Some(sel!(hideToTray:)));
            }
        }
        let center = NSNotificationCenter::defaultCenter();
        let observe = |name, f: fn(MainThreadMarker)| {
            let block = RcBlock::new(move |_: NonNull<NSNotification>| f(MainThreadMarker::new().expect("main thread")));
            // SAFETY: posted on the main thread (no queue given); removed in `uninstall`.
            unsafe { center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &block) }
        };
        let observers = vec![
            // Minimized another way (the Dock, another shortcut): into the tray instead.
            observe(unsafe { NSWindowDidMiniaturizeNotification }, |mtm| {
                if let Some(w) = window(mtm) {
                    w.deminiaturize(None);
                }
                hide(mtm);
            }),
            // Brought back another way (opening the app again): out of the tray.
            observe(unsafe { NSWindowDidBecomeKeyNotification }, |mtm| {
                if HIDDEN.with(|h| h.borrow_mut().take()).is_some() {
                    NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Regular);
                }
            }),
        ];
        TRAY.with(|t| *t.borrow_mut() = Some(Tray { item, target, observers }));
    }

    fn uninstall(mtm: MainThreadMarker) {
        if is_hidden() {
            show(mtm);
        }
        let Some(tray) = TRAY.with(|t| t.borrow_mut().take()) else { return };
        NSStatusBar::systemStatusBar().removeStatusItem(&tray.item);
        for o in &tray.observers {
            // SAFETY: each was added by `install`.
            unsafe { NSNotificationCenter::defaultCenter().removeObserver(AsRef::<AnyObject>::as_ref(&**o)) };
        }
        if let Some(w) = window(mtm) {
            if let Some(b) = w.standardWindowButton(NSWindowButton::MiniaturizeButton) {
                // SAFETY: the window outlives its own button.
                unsafe {
                    b.setTarget(Some(&w));
                    b.setAction(Some(sel!(performMiniaturize:)));
                }
            }
        }
        drop(tray.target);
    }

    pub const AVAILABLE: bool = true;

    pub fn set_enabled(on: bool) {
        on_main(move |mtm| if on { install(mtm) } else { uninstall(mtm) });
    }

    /// Minimize: into the tray in tray mode, else to the Dock.
    pub fn minimize() {
        on_main(hide);
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub const AVAILABLE: bool = false;

    pub fn set_enabled(_on: bool) {}

    pub fn minimize() {
        nsh_core::element::desktop::minimize();
    }
}

pub use imp::{minimize, set_enabled, AVAILABLE};
