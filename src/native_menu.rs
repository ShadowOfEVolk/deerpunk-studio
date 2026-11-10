//! Native macOS "File" menu (Open Project…, Open Recent ▸) installed into
//! NSApp.mainMenu via objc2-app-kit. Menu item clicks are queued in a static
//! deque and drained from the eframe update loop on the main thread.

#[derive(Debug, Clone)]
pub enum NativeMenuAction {
    OpenProject,
    /// Absolute path chosen from the "Open Recent" submenu.
    OpenRecent(String),
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::NativeMenuAction;

    pub fn install() {}
    pub fn set_recents(_paths: &[String]) {}
    pub fn poll_actions() -> Vec<NativeMenuAction> {
        Vec::new()
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::collections::VecDeque;
    use std::sync::LazyLock;
    use std::sync::Mutex;

    use objc2::{declare_class, msg_send_id, mutability, ClassType, DeclaredClass};
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObject};
    use objc2::sel;
    use objc2_app_kit::{NSApplication, NSMenu, NSMenuItem, NSEventModifierFlags};
    use objc2_foundation::{ns_string, MainThreadMarker, NSString};

    use super::NativeMenuAction;

    /// Actions pushed by menu item clicks (main thread), drained by
    /// `poll_actions` from the eframe update loop (also main thread).
    static ACTIONS: Mutex<VecDeque<NativeMenuAction>> = Mutex::new(VecDeque::new());

    /// The single ObjC target handling all our menu-item actions. Created
    /// lazily on first access, which only happens on the main thread. The
    /// object itself is main-thread-only; the wrapper only makes the static
    /// slot usable — all access goes through `target(mtm)`.
    static TARGET: LazyLock<Option<MainThreadTarget>> =
        LazyLock::new(|| MenuTarget::maybe_new().map(MainThreadTarget));

    struct MainThreadTarget(Retained<MenuTarget>);
    // SAFETY: the target is created on, and only ever dereferenced from, the
    // main thread (guarded by MainThreadMarker::new() at every use site).
    unsafe impl Send for MainThreadTarget {}
    unsafe impl Sync for MainThreadTarget {}

    fn target(mtm: MainThreadMarker) -> Option<&'static Retained<MenuTarget>> {
        let _ = mtm; // documents the main-thread requirement for callers
        TARGET.as_ref().map(|t| &t.0)
    }

    declare_class!(
        struct MenuTarget;

        unsafe impl ClassType for MenuTarget {
            type Super = NSObject;
            type Mutability = mutability::MainThreadOnly;
            const NAME: &'static str = "DeerpunkMenuTarget";
        }

        impl DeclaredClass for MenuTarget {}

        unsafe impl MenuTarget {
            // File → Open Project…
            #[method(openProject:)]
            fn open_project(&self, _sender: Option<&NSMenuItem>) {
                ACTIONS
                    .lock()
                    .unwrap()
                    .push_back(NativeMenuAction::OpenProject);
            }

            // File → Open Recent ▸ → <path>; the path rides in representedObject.
            #[method(openRecent:)]
            fn open_recent(&self, item: Option<&NSMenuItem>) {
                let Some(item) = item else { return };
                // SAFETY: plain getter; item is the NSMenuItem we created.
                let represented: Option<Retained<AnyObject>> =
                    unsafe { item.representedObject() };
                let Some(represented) = represented else { return };
                // SAFETY: we only ever store NSString as representedObject,
                // so this cast is valid.
                let path: Retained<NSString> = unsafe { Retained::cast(represented) };
                ACTIONS
                    .lock()
                    .unwrap()
                    .push_back(NativeMenuAction::OpenRecent(path.to_string()));
            }
        }
    );

    impl MenuTarget {
        fn maybe_new() -> Option<Retained<Self>> {
            let _mtm = MainThreadMarker::new()?;
            // SAFETY: instantiating our own declared class on the main thread.
            Some(unsafe { msg_send_id![MenuTarget::class(), new] })
        }
    }

    fn menu_item(
        mtm: MainThreadMarker,
        title: &NSString,
        action: Option<objc2::runtime::Sel>,
        key: &'static NSString,
    ) -> Retained<NSMenuItem> {
        // SAFETY: plain NSMenuItem construction on the main thread.
        unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(mtm.alloc(), title, action, key)
        }
    }

    /// Remove any previously installed "File" menu (our own re-run) so the
    /// install is idempotent.
    fn remove_existing_file_menu(main_menu: &NSMenu) {
        // SAFETY: numberOfItems/itemAtIndex are plain getters; removal is safe.
        unsafe {
            let count = main_menu.numberOfItems();
            let mut victim: Option<Retained<NSMenuItem>> = None;
            for i in 0..count {
                if let Some(item) = main_menu.itemAtIndex(i) {
                    let title = item.title();
                    if title.to_string() == "File" {
                        victim = Some(item);
                        break;
                    }
                }
            }
            if let Some(item) = victim {
                main_menu.removeItem(&item);
            }
        }
    }

    pub fn install() {
        let Some(mtm) = MainThreadMarker::new() else {
            // Not on the main thread: the menubar cannot be touched safely.
            return;
        };
        // SAFETY: sharedApplication on the main thread.
        let app = NSApplication::sharedApplication(mtm);
        // winit installs a main menu before running; without one there is
        // nothing to attach to.
        let Some(main_menu) = (unsafe { app.mainMenu() }) else {
            return;
        };
        let Some(target) = target(mtm) else { return };

        remove_existing_file_menu(&main_menu);

        let file_menu = NSMenu::new(mtm);
        unsafe { file_menu.setTitle(ns_string!("File")) };

        // File → Open Project… (⌘O).
        let open_item = menu_item(
            mtm,
            ns_string!("Open Project…"),
            Some(sel!(openProject:)),
            ns_string!("o"),
        );
        open_item
            .setKeyEquivalentModifierMask(NSEventModifierFlags::NSEventModifierFlagCommand);
        // SAFETY: our target lives forever in the TARGET static.
        unsafe { open_item.setTarget(Some(target)) };

        // File → Open Recent ▸ (rebuilt later by set_recents).
        let recents_menu = NSMenu::new(mtm);
        unsafe { recents_menu.setTitle(ns_string!("Open Recent")) };
        let recents_holder = menu_item(mtm, ns_string!("Open Recent"), None, ns_string!(""));
        recents_holder.setSubmenu(Some(&recents_menu));

        file_menu.addItem(&open_item);
        file_menu.addItem(&recents_holder);

        // Wrap the File menu in a holder item and insert it right after the
        // application menu (index 1).
        let file_holder = menu_item(mtm, ns_string!("File"), None, ns_string!(""));
        file_holder.setSubmenu(Some(&file_menu));
        // SAFETY: index 1 always exists: winit's app menu sits at index 0.
        unsafe { main_menu.insertItem_atIndex(&file_holder, 1) };
    }

    pub fn set_recents(paths: &[String]) {
        let Some(mtm) = MainThreadMarker::new() else { return };
        // SAFETY: sharedApplication on the main thread.
        let app = NSApplication::sharedApplication(mtm);
        let Some(main_menu) = (unsafe { app.mainMenu() }) else { return };
        let Some(target) = target(mtm) else { return };

        let Some(file_holder) = (unsafe { main_menu.itemWithTitle(ns_string!("File")) })
        else {
            return;
        };
        let Some(file_menu) = (unsafe { file_holder.submenu() }) else { return };
        let Some(recents_holder) =
            (unsafe { file_menu.itemWithTitle(ns_string!("Open Recent")) })
        else {
            return;
        };
        let Some(recents_menu) = (unsafe { recents_holder.submenu() }) else { return };
        // SAFETY: clearing the current recent items before rebuilding.
        unsafe { recents_menu.removeAllItems() };

        if paths.is_empty() {
            let empty = menu_item(mtm, ns_string!("No recent projects"), None, ns_string!(""));
            unsafe { empty.setEnabled(false) };
            recents_menu.addItem(&empty);
            return;
        }

        for path in paths {
            let title = NSString::from_str(path);
            let item = menu_item(mtm, &title, Some(sel!(openRecent:)), ns_string!(""));
            let repr = NSString::from_str(path);
            let repr_any: Retained<AnyObject> = unsafe { Retained::cast(repr) };
            // SAFETY: our target lives forever in the TARGET static.
            unsafe { item.setTarget(Some(target)) };
            // SAFETY: setter on our own item.
            unsafe { item.setRepresentedObject(Some(&repr_any)) };
            recents_menu.addItem(&item);
        }
    }

    pub fn poll_actions() -> Vec<NativeMenuAction> {
        ACTIONS.lock().unwrap().drain(..).collect()
    }
}

pub use imp::{install, poll_actions, set_recents};
