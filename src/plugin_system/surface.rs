use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static HOST_APPEARANCE: AtomicU8 = AtomicU8::new(0);

pub(crate) fn set_host_appearance(appearance: Appearance) {
    let value = match appearance {
        Appearance::Light => 1,
        Appearance::Dark => 2,
        Appearance::System => 0,
    };
    HOST_APPEARANCE.store(value, Ordering::Release);
}

fn host_appearance() -> Appearance {
    match HOST_APPEARANCE.load(Ordering::Acquire) {
        1 => Appearance::Light,
        2 => Appearance::Dark,
        _ => Appearance::System,
    }
}

use nsh_core::{
    platform::{Appearance, Locale},
    storage::{StorageBackend, StorageChange},
};
use nsh_engine::{App, Bundle, FileStorage};
#[cfg(target_os = "macos")]
use nsh_platform_macos as platform;
#[cfg(windows)]
use nsh_platform_windows as platform;
use platform::{AppOptions, AppView, AssetBase, ComponentContext, Components, NativeComponent};
use serde_json::{Value as JsonValue, json};

use super::host_api::{self, SurfaceDescriptor, SurfaceMount};
use super::loader::{installed_package_path, load_verified};

pub const PLUGIN_SURFACE: &str = "pig.pluginSurface";

pub fn register(components: &Components) {
    components.register(
        PLUGIN_SURFACE,
        Rc::new(|context| Box::new(PluginSurface::new(context))),
    );
}

#[derive(Clone)]
struct SurfaceProps {
    surface: String,
    theme: Option<String>,
    locale: Option<Locale>,
    appearance: Appearance,
    width: Option<f32>,
    height: Option<f32>,
}

impl Default for SurfaceProps {
    fn default() -> Self {
        Self {
            surface: String::new(),
            theme: None,
            locale: None,
            appearance: Appearance::System,
            width: None,
            height: None,
        }
    }
}

struct LoadedSurface {
    bundle: Bundle,
    assets: PathBuf,
}

struct LoadResult {
    signature: String,
    result: Result<LoadedSurface, String>,
}

#[derive(Default)]
struct SurfaceKv {
    current: Mutex<Option<Arc<FileStorage>>>,
}

impl SurfaceKv {
    fn bind(&self, directory: &Path) {
        *self.current.lock().expect("surface storage lock") = Some(FileStorage::open(directory));
    }

    fn clear(&self) {
        *self.current.lock().expect("surface storage lock") = None;
    }

    fn current(&self) -> Option<Arc<FileStorage>> {
        self.current.lock().expect("surface storage lock").clone()
    }
}

impl StorageBackend for SurfaceKv {
    fn load(&self) -> Result<BTreeMap<String, String>, String> {
        match self.current() {
            Some(storage) => storage.load(),
            None => Ok(BTreeMap::new()),
        }
    }

    fn write(&self, change: StorageChange) {
        if let Some(storage) = self.current() {
            storage.write(change);
        }
    }
}

struct PluginSurface {
    context: ComponentContext,
    props: RefCell<SurfaceProps>,
    mount: RefCell<Option<SurfaceMount>>,
    storage: Arc<SurfaceKv>,
    view: RefCell<Option<Rc<AppView>>>,
    requested: RefCell<Option<String>>,
    results: Arc<Mutex<Vec<LoadResult>>>,
    environment: RefCell<Option<Appearance>>,
    applied_size: RefCell<Option<(f32, f32)>>,
    #[cfg(windows)]
    container: Option<platform::windows::Win32::Foundation::HWND>,
}

impl PluginSurface {
    fn new(context: ComponentContext) -> Self {
        let limits = nsh_engine::Limits {
            memory_bytes: 64 * 1024 * 1024,
            load_budget: Duration::from_secs(3),
            call_budget: Duration::from_millis(750),
            ..nsh_engine::Limits::default()
        };
        let storage = Arc::new(SurfaceKv::default());
        let options = AppOptions {
            app_id: "com.example.pigcodets".into(),
            title: "Pig Code plugin".into(),
            appearance: host_appearance(),
            storage: Some(storage.clone()),
            limits,
            ..AppOptions::default()
        };
        #[cfg(windows)]
        let container = container::create().ok();
        #[cfg(windows)]
        let view = container.and_then(|parent| {
            let view = AppView::new(Some(parent), options).ok()?;
            container::attach(parent, view.hwnd()).then_some(view)
        });
        #[cfg(target_os = "macos")]
        let view = platform::objc2::MainThreadMarker::new().map(|mtm| AppView::new(mtm, options));
        Self {
            context,
            props: RefCell::new(SurfaceProps::default()),
            mount: RefCell::new(None),
            storage,
            view: RefCell::new(view),
            requested: RefCell::new(None),
            results: Arc::new(Mutex::new(Vec::new())),
            environment: RefCell::new(None),
            applied_size: RefCell::new(None),
            #[cfg(windows)]
            container,
        }
    }

    fn sync(&self) {
        let size = {
            let props = self.props.borrow();
            (props.width, props.height)
        };
        if let (Some(width), Some(height)) = size
            && *self.applied_size.borrow() != Some((width, height))
        {
            self.context.set_size(width, height);
            *self.applied_size.borrow_mut() = Some((width, height));
        }
        self.attach();
        self.sync_host_environment();
        self.start_load();
        self.finish_loads();
    }

    fn sync_host_environment(&self) {
        let appearance = self.props.borrow().appearance;
        let Some(view) = self.view.borrow().as_ref().cloned() else {
            return;
        };
        if self.mount.borrow().is_none() {
            return;
        }
        let name = match appearance {
            Appearance::Light => "light",
            Appearance::Dark => "dark",
            Appearance::System => "system",
        };
        let mut params = view.host().params();
        let Some(plugin) = params.get_mut("plugin").and_then(JsonValue::as_object_mut) else {
            return;
        };
        plugin.insert("appearance".into(), name.into());
        view.host().set_params(params);
        if self.environment.borrow().as_ref() != Some(&appearance) {
            let _ = view
                .host()
                .send("pig.host.environment", json!({ "appearance": name }));
            *self.environment.borrow_mut() = Some(appearance);
        }
    }

    fn attach(&self) {
        let id = self.props.borrow().surface.clone();
        let attached = self
            .mount
            .borrow()
            .as_ref()
            .is_some_and(|mount| mount.descriptor.surface == id);
        if id.is_empty() || attached {
            return;
        }
        self.release();
        let Some(view) = self.view.borrow().as_ref().cloned() else {
            self.report_error("could not create the plugin AppView".into());
            return;
        };
        match host_api::attach(&id, view.host()) {
            Ok(mount) => {
                self.storage.bind(&mount.descriptor.storage_dir);
                *self.mount.borrow_mut() = Some(mount);
            }
            Err(error) => self.report_error(error),
        }
    }

    fn descriptor(&self) -> Option<SurfaceDescriptor> {
        self.mount
            .borrow()
            .as_ref()
            .map(|mount| mount.descriptor.clone())
    }

    fn signature(descriptor: &SurfaceDescriptor) -> String {
        format!(
            "{}:{}:{}:{}",
            descriptor.package_root.display(),
            descriptor.instance,
            descriptor.contribution,
            descriptor.revision
        )
    }

    fn start_load(&self) {
        let Some(descriptor) = self.descriptor() else {
            return;
        };
        let signature = Self::signature(&descriptor);
        if self.requested.borrow().as_deref() == Some(signature.as_str()) {
            return;
        }
        *self.requested.borrow_mut() = Some(signature.clone());
        let root = descriptor.package_root;
        let package = installed_package_path(&root);
        let trust = package_trust_root(&root);
        let data_dir = package_data_dir(&root).unwrap_or_else(|| root.clone());
        let results = self.results.clone();
        let context = self.context.clone();
        std::thread::spawn(move || {
            let result = load_verified(&package, &trust, &data_dir).map(|verified| LoadedSurface {
                bundle: verified.bundle,
                assets: verified.assets,
            });
            results
                .lock()
                .expect("plugin surface result lock")
                .push(LoadResult { signature, result });
            #[cfg(windows)]
            context.redraw();
            #[cfg(target_os = "macos")]
            context.emit("sync", json!({}));
        });
    }

    fn finish_loads(&self) {
        let results =
            std::mem::take(&mut *self.results.lock().expect("plugin surface result lock"));
        for load in results {
            if self.requested.borrow().as_deref() != Some(load.signature.as_str()) {
                continue;
            }
            match load.result {
                Ok(loaded) => {
                    let Some(view) = self.view.borrow().as_ref().cloned() else {
                        continue;
                    };
                    let props = self.props.borrow().clone();
                    self.apply_theme(&view, props.theme.as_deref());
                    view.set_locale(props.locale);
                    view.set_asset_base(Some(AssetBase::Directory(loaded.assets.clone())));
                    view.load(App::Js(loaded.bundle.clone()));
                    if let Some(descriptor) = self.descriptor() {
                        self.context.emit(
                            "ready",
                            json!({
                                "surface": descriptor.surface,
                                "instance": descriptor.instance,
                                "contribution": descriptor.contribution,
                                "revision": descriptor.revision,
                            }),
                        );
                    }
                }
                Err(error) => self.report_error(error),
            }
        }
    }

    fn apply_theme(&self, view: &AppView, theme: Option<&str>) {
        if let Err(error) = view.set_theme_overrides(theme) {
            self.report_error(error.to_string());
        }
    }

    fn report_error(&self, message: String) {
        self.context.emit("error", json!({ "message": message }));
    }

    fn release(&self) {
        #[cfg(windows)]
        if let Some(container) = self.container {
            container::reset(container);
        }
        if let Some(mount) = self.mount.borrow_mut().take() {
            mount.manager.unmount_surface(&mount.descriptor.surface);
        }
        if let Some(view) = self.view.borrow().as_ref() {
            view.stop();
        }
        self.storage.clear();
        *self.requested.borrow_mut() = None;
    }
}

impl NativeComponent for PluginSurface {
    #[cfg(windows)]
    fn draw(
        &self,
        dc: &platform::windows::Win32::Graphics::Direct2D::ID2D1DeviceContext,
        size: (f32, f32),
    ) {
        let _ = (dc, size);
        self.sync();
    }

    #[cfg(windows)]
    fn window(&self) -> Option<platform::windows::Win32::Foundation::HWND> {
        self.container
    }

    #[cfg(target_os = "macos")]
    fn view(&self) -> platform::objc2::rc::Retained<platform::objc2_app_kit::NSView> {
        self.view
            .borrow()
            .as_ref()
            .expect("macOS plugin AppView")
            .view()
    }

    fn set_prop(&mut self, name: &str, value: Option<&JsonValue>) {
        let mut props = self.props.borrow_mut();
        match name {
            "surface" => {
                props.surface = value.and_then(JsonValue::as_str).unwrap_or_default().into();
            }
            "theme" => {
                props.theme = value.and_then(JsonValue::as_str).map(str::to_owned);
                if let Some(view) = self.view.borrow().as_ref() {
                    self.apply_theme(view, props.theme.as_deref());
                }
            }
            "locale" => {
                props.locale = value.and_then(JsonValue::as_str).and_then(Locale::parse);
                if let Some(view) = self.view.borrow().as_ref() {
                    view.set_locale(props.locale);
                }
            }
            "appearance" => {
                props.appearance = match value.and_then(JsonValue::as_str) {
                    Some("light") => Appearance::Light,
                    Some("dark") => Appearance::Dark,
                    _ => Appearance::System,
                };
            }
            "width" => {
                props.width = value.and_then(JsonValue::as_f64).map(|width| width as f32);
            }
            "height" => {
                props.height = value
                    .and_then(JsonValue::as_f64)
                    .map(|height| height as f32);
            }
            _ => {}
        }
        drop(props);
        self.sync();
    }

    fn command(&mut self, name: &str, _args: &JsonValue) {
        match name {
            "reload" => {
                *self.requested.borrow_mut() = None;
                self.sync();
            }
            "sync" => self.finish_loads(),
            _ => {}
        }
    }
}

impl Drop for PluginSurface {
    fn drop(&mut self) {
        self.release();
        if let Some(view) = self.view.borrow_mut().take() {
            view.stop();
        }
        #[cfg(windows)]
        if let Some(container) = self.container {
            unsafe { container::destroy(container) };
        }
    }
}

pub(super) use super::loader::{package_data_dir, package_trust_root};

#[cfg(windows)]
mod container {
    use super::platform;
    use platform::windows::{
        Win32::{
            Foundation::{HWND, LPARAM, LRESULT, WPARAM},
            UI::{
                Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
                WindowsAndMessaging::{
                    CreateWindowExW, DestroyWindow, GWLP_USERDATA, GetWindowLongPtrW, MoveWindow,
                    SW_HIDE, SW_SHOW, SendMessageW, SetWindowLongPtrW, ShowWindow, WM_APP,
                    WM_NCDESTROY, WM_PAINT, WM_SIZE, WM_WINDOWPOSCHANGED, WS_CLIPSIBLINGS,
                    WS_POPUP,
                },
            },
        },
        core::w,
    };

    const NSH_WAKE: u32 = WM_APP + 1;
    const READY: isize = 2;

    pub fn create() -> platform::windows::core::Result<HWND> {
        unsafe {
            CreateWindowExW(
                Default::default(),
                w!("STATIC"),
                w!(""),
                WS_POPUP | WS_CLIPSIBLINGS,
                -10000,
                -10000,
                1,
                1,
                None,
                None,
                None,
                None,
            )
        }
    }

    pub fn attach(parent: HWND, child: HWND) -> bool {
        unsafe {
            SetWindowSubclass(parent, Some(resize), parent.0 as usize, child.0 as usize).as_bool()
                && SetWindowSubclass(child, Some(content), child.0 as usize, parent.0 as usize)
                    .as_bool()
        }
    }

    pub fn reset(parent: HWND) {
        unsafe {
            SetWindowLongPtrW(parent, GWLP_USERDATA, 0);
            let _ = ShowWindow(parent, SW_HIDE);
        }
    }

    pub unsafe fn destroy(parent: HWND) {
        unsafe {
            let _ = DestroyWindow(parent);
        }
    }

    fn state(parent: HWND) -> isize {
        unsafe { GetWindowLongPtrW(parent, GWLP_USERDATA) }
    }

    unsafe extern "system" fn resize(
        parent: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        id: usize,
        child: usize,
    ) -> LRESULT {
        unsafe {
            if message == WM_WINDOWPOSCHANGED && state(parent) != READY {
                let _ = ShowWindow(parent, SW_HIDE);
            } else if message == WM_SIZE {
                let width = lparam.0 as u32 & 0xffff;
                let height = (lparam.0 as u32 >> 16) & 0xffff;
                let _ = MoveWindow(
                    HWND(child as *mut std::ffi::c_void),
                    0,
                    0,
                    width as i32,
                    height as i32,
                    true,
                );
            } else if message == WM_NCDESTROY {
                let _ = RemoveWindowSubclass(parent, Some(resize), id);
            }
            DefSubclassProc(parent, message, wparam, lparam)
        }
    }

    unsafe extern "system" fn content(
        child: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        id: usize,
        parent: usize,
    ) -> LRESULT {
        let parent = HWND(parent as *mut std::ffi::c_void);
        let result = unsafe { DefSubclassProc(child, message, wparam, lparam) };
        unsafe {
            if message == NSH_WAKE && state(parent) == 0 {
                SetWindowLongPtrW(parent, GWLP_USERDATA, 1);
                let _ = SendMessageW(child, WM_PAINT, Some(WPARAM(0)), Some(LPARAM(0)));
            } else if message == WM_PAINT && state(parent) == 1 {
                SetWindowLongPtrW(parent, GWLP_USERDATA, READY);
                let _ = ShowWindow(parent, SW_SHOW);
            } else if message == WM_NCDESTROY {
                let _ = RemoveWindowSubclass(child, Some(content), id);
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_paths_resolve_to_the_shared_data_and_trust_roots() {
        let root = PathBuf::from("C:/data/packages/demo/digest/assets");
        assert_eq!(package_data_dir(&root), Some(PathBuf::from("C:/data")));
        assert_eq!(
            package_trust_root(&root),
            PathBuf::from("C:/data/plugin-root.pub")
        );
        assert_eq!(
            installed_package_path(&root),
            root.join(".pig-package.nshpkg")
        );
    }

    #[test]
    fn surface_identity_is_bound_to_the_manager_allocated_revision() {
        let descriptor = SurfaceDescriptor {
            instance: "demo:digest:user:default".into(),
            surface: "surface-1".into(),
            contribution: "panel".into(),
            revision: 1,
            package_root: PathBuf::from("C:/data/packages/demo/digest/assets"),
            storage_dir: PathBuf::from("C:/data/instances/stable/nsh"),
            session_id: None,
            turn_id: None,
        };
        let next = SurfaceDescriptor {
            revision: 2,
            surface: "surface-2".into(),
            ..descriptor.clone()
        };
        assert_ne!(
            PluginSurface::signature(&descriptor),
            PluginSurface::signature(&next)
        );
        assert_eq!(
            package_data_dir(&descriptor.package_root),
            package_data_dir(&next.package_root)
        );
        assert_eq!(descriptor.storage_dir, next.storage_dir);
        assert!(!descriptor.storage_dir.to_string_lossy().contains("digest"));
    }

    #[test]
    fn native_storage_is_empty_until_bound_to_the_stable_data_directory() {
        let storage = SurfaceKv::default();
        assert!(storage.current().is_none());
        assert!(storage.load().unwrap().is_empty());
        let directory = PathBuf::from("C:/data/instances/stable/nsh");
        storage.bind(&directory);
        assert_eq!(
            storage.current().unwrap().path(),
            directory.join("storage.json")
        );
        storage.clear();
        assert!(storage.current().is_none());
        assert!(storage.load().unwrap().is_empty());
    }
}
