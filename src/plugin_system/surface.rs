use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nsh_core::platform::{Appearance, Locale};
use nsh_engine::{App, Bundle, HostChannel};
use nsh_platform_windows::{
    AppOptions, AppView, AssetBase, ComponentContext, Components, NativeComponent,
};
use serde_json::{Value as JsonValue, json};

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
    instance: String,
    contribution: String,
    revision: u64,
    package_root: Option<PathBuf>,
    theme: Option<String>,
    locale: Option<Locale>,
    appearance: Appearance,
}

impl Default for SurfaceProps {
    fn default() -> Self {
        Self {
            instance: String::new(),
            contribution: String::new(),
            revision: 0,
            package_root: None,
            theme: None,
            locale: None,
            appearance: Appearance::System,
        }
    }
}

impl SurfaceProps {
    fn signature(&self) -> Option<String> {
        Some(format!(
            "{}:{}:{}:{}",
            self.package_root.as_deref()?.display(),
            self.instance,
            self.contribution,
            self.revision
        ))
    }

    fn host_params(&self) -> serde_json::Map<String, JsonValue> {
        json!({
            "plugin": {
                "instance": self.instance,
                "contribution": self.contribution,
                "revision": self.revision,
                "epoch": 0,
                "generation": self.revision,
            }
        })
        .as_object()
        .expect("surface host params")
        .clone()
    }
}

struct LoadedSurface {
    signature: String,
    bundle: Bundle,
    assets: PathBuf,
}

struct LoadResult {
    signature: String,
    result: Result<LoadedSurface, String>,
}

struct PluginSurface {
    context: ComponentContext,
    props: RefCell<SurfaceProps>,
    view: RefCell<Option<Rc<AppView>>>,
    requested: RefCell<Option<String>>,
    loaded: RefCell<Option<LoadedSurface>>,
    results: Arc<Mutex<Vec<LoadResult>>>,
}

impl PluginSurface {
    fn new(context: ComponentContext) -> Self {
        let view = AppView::new(None, AppOptions::default()).ok();
        if let Some(view) = &view {
            view.host().handle("plugin.protocol", |call| {
                call.resolve(JsonValue::Null);
            });
        }
        Self {
            context,
            props: RefCell::new(SurfaceProps::default()),
            view: RefCell::new(view),
            requested: RefCell::new(None),
            loaded: RefCell::new(None),
            results: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn sync(&self) {
        self.ensure_view();
        self.start_load();
        self.finish_loads();
    }

    fn ensure_view(&self) {
        if self.view.borrow().is_some() {
            return;
        }
        let props = self.props.borrow().clone();
        let Some(root) = props.package_root.clone() else {
            return;
        };
        let host = HostChannel::new();
        host.set_params(props.host_params());
        host.handle("plugin.protocol", |call| {
            call.resolve(JsonValue::Null);
        });
        let mut limits = nsh_engine::Limits::default();
        limits.memory_bytes = 64 * 1024 * 1024;
        limits.load_budget = Duration::from_secs(3);
        limits.call_budget = Duration::from_millis(750);
        let options = AppOptions {
            app_id: "com.example.pigcodets".into(),
            title: "Pig Code plugin".into(),
            appearance: props.appearance,
            asset_base: Some(AssetBase::Directory(root)),
            data_dir: package_data_dir(&props).map(|path| path.join("surfaces")),
            host,
            limits,
            ..AppOptions::default()
        };
        match AppView::new(None, options) {
            Ok(view) => {
                if let Some(theme) = props.theme.as_deref() {
                    self.apply_theme(view.as_ref(), Some(theme));
                }
                view.set_locale(props.locale);
                self.context.set_size(320.0, 240.0);
                *self.view.borrow_mut() = Some(view);
                self.load_retained(&props);
            }
            Err(error) => self.report_error(error.to_string()),
        }
    }

    fn start_load(&self) {
        let Some(signature) = self.props.borrow().signature() else {
            return;
        };
        if self.requested.borrow().as_deref() == Some(signature.as_str()) {
            return;
        }
        *self.requested.borrow_mut() = Some(signature.clone());
        if let Some(view) = self.view.borrow().as_ref() {
            view.stop();
        }
        let props = self.props.borrow().clone();
        let Some(root) = props.package_root.clone() else {
            return;
        };
        let package = installed_package_path(&root);
        let trust = package_trust_root(&root);
        let data_dir = package_data_dir(&props).unwrap_or_else(|| root.clone());
        let results = self.results.clone();
        let context = self.context.clone();
        std::thread::spawn(move || {
            let result = load_verified(&package, &trust, &data_dir).map(|verified| LoadedSurface {
                signature: signature.clone(),
                bundle: verified.bundle,
                assets: verified.assets,
            });
            results
                .lock()
                .expect("plugin surface result lock")
                .push(LoadResult { signature, result });
            context.redraw();
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
                    self.ensure_view();
                    let Some(view) = self.view.borrow().as_ref().cloned() else {
                        continue;
                    };
                    let props = self.props.borrow().clone();
                    self.apply_theme(&view, props.theme.as_deref());
                    view.set_locale(props.locale);
                    view.set_asset_base(Some(AssetBase::Directory(loaded.assets.clone())));
                    view.host().set_params(props.host_params());
                    view.load(App::Js(loaded.bundle.clone()));
                    *self.loaded.borrow_mut() = Some(loaded);
                    self.context.emit(
                        "ready",
                        json!({
                            "instance": props.instance,
                            "contribution": props.contribution,
                            "revision": props.revision,
                        }),
                    );
                }
                Err(error) => self.report_error(error),
            }
        }
    }

    fn load_retained(&self, props: &SurfaceProps) {
        let Some(signature) = props.signature() else {
            return;
        };
        let Some(view) = self.view.borrow().as_ref().cloned() else {
            return;
        };
        let loaded = self.loaded.borrow();
        let Some(loaded) = loaded
            .as_ref()
            .filter(|loaded| loaded.signature == signature)
        else {
            return;
        };
        view.set_asset_base(Some(AssetBase::Directory(loaded.assets.clone())));
        view.host().set_params(props.host_params());
        view.load(App::Js(loaded.bundle.clone()));
        self.context.emit(
            "ready",
            json!({
                "instance": props.instance,
                "contribution": props.contribution,
                "revision": props.revision,
            }),
        );
    }

    fn apply_theme(&self, view: &AppView, theme: Option<&str>) {
        if let Err(error) = view.set_theme_overrides(theme) {
            self.report_error(error.to_string());
        }
    }

    fn report_error(&self, message: String) {
        self.context.emit("error", json!({ "message": message }));
    }
}

impl NativeComponent for PluginSurface {
    fn draw(
        &self,
        dc: &nsh_platform_windows::windows::Win32::Graphics::Direct2D::ID2D1DeviceContext,
        size: (f32, f32),
    ) {
        let _ = (dc, size);
        self.sync();
    }

    fn window(&self) -> Option<nsh_platform_windows::windows::Win32::Foundation::HWND> {
        self.sync();
        self.view.borrow().as_ref().map(|view| view.hwnd())
    }

    fn set_prop(&mut self, name: &str, value: Option<&JsonValue>) {
        let mut props = self.props.borrow_mut();
        match name {
            "instance" => {
                props.instance = value.and_then(JsonValue::as_str).unwrap_or_default().into();
            }
            "contribution" => {
                props.contribution = value.and_then(JsonValue::as_str).unwrap_or_default().into();
            }
            "revision" => {
                props.revision = value.and_then(JsonValue::as_u64).unwrap_or_default();
            }
            "packageRoot" => {
                props.package_root = value
                    .and_then(JsonValue::as_str)
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from);
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
            _ => {}
        }
        drop(props);
        self.sync();
    }

    fn command(&mut self, name: &str, _args: &JsonValue) {
        if name == "reload" {
            *self.requested.borrow_mut() = None;
            self.sync();
        }
    }
}

impl Drop for PluginSurface {
    fn drop(&mut self) {
        if let Some(view) = self.view.borrow_mut().take() {
            view.stop();
        }
    }
}

fn package_data_dir(props: &SurfaceProps) -> Option<PathBuf> {
    props
        .package_root
        .as_deref()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(Path::to_path_buf)
}

fn package_trust_root(root: &Path) -> PathBuf {
    let packaged = root.join(".pig-root.pub");
    if packaged.is_file() {
        return packaged;
    }
    root.parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(|path| path.join("plugin-root.pub"))
        .unwrap_or(packaged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_paths_resolve_to_the_shared_data_and_trust_roots() {
        let root = PathBuf::from("C:/data/packages/demo/digest/assets");
        let props = SurfaceProps {
            package_root: Some(root.clone()),
            ..SurfaceProps::default()
        };
        assert_eq!(package_data_dir(&props), Some(PathBuf::from("C:/data")));
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
    fn revision_changes_the_surface_but_not_the_stable_data_directory() {
        let root = PathBuf::from("C:/data/packages/demo/digest/assets");
        let props = SurfaceProps {
            instance: "demo:digest:user:default".into(),
            contribution: "panel".into(),
            revision: 1,
            package_root: Some(root.clone()),
            ..SurfaceProps::default()
        };
        let next = SurfaceProps {
            revision: 2,
            ..props.clone()
        };
        assert_ne!(props.signature(), next.signature());
        assert_eq!(package_data_dir(&props), package_data_dir(&next));
    }
}
