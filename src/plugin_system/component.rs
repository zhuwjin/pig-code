use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use nsh_engine::HostChannel;
#[cfg(target_os = "linux")]
use nsh_platform_linux as platform;
#[cfg(target_os = "macos")]
use nsh_platform_macos as platform;
#[cfg(windows)]
use nsh_platform_windows as platform;
use platform::{ComponentContext, Components, NativeComponent};
use serde_json::{Value as JsonValue, json};

use super::host_api::{self, SurfaceMount};
use super::loader::{installed_package_path, load_verified, package_data_dir, package_trust_root};

pub const PLUGIN_COMPONENT: &str = "pig.pluginComponent";

pub fn register(components: &Components) {
    components.register(
        PLUGIN_COMPONENT,
        std::rc::Rc::new(|context| Box::new(PluginComponent::new(context))),
    );
}

#[derive(Default)]
struct Props {
    surface: String,
}

struct PluginComponent {
    context: ComponentContext,
    props: RefCell<Props>,
    mount: RefCell<Option<SurfaceMount>>,
    requested: Arc<Mutex<Option<String>>>,
    #[cfg(target_os = "linux")]
    widget: platform::gtk4::Widget,
    #[cfg(target_os = "macos")]
    view: platform::objc2::rc::Retained<platform::objc2_app_kit::NSView>,
}

impl PluginComponent {
    fn new(context: ComponentContext) -> Self {
        #[cfg(target_os = "linux")]
        let widget = platform::gtk4::DrawingArea::new().into();
        #[cfg(target_os = "macos")]
        let view = {
            let mtm = platform::objc2::MainThreadMarker::new()
                .expect("native components are created on the main thread");
            platform::objc2_app_kit::NSView::new(mtm)
        };
        Self {
            context,
            props: RefCell::new(Props::default()),
            mount: RefCell::new(None),
            requested: Arc::new(Mutex::new(None)),
            #[cfg(target_os = "linux")]
            widget,
            #[cfg(target_os = "macos")]
            view,
        }
    }

    fn sync(&self) {
        self.attach();
        self.start_load();
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
        match host_api::attach(&id, HostChannel::default()) {
            Ok(mount) => *self.mount.borrow_mut() = Some(mount),
            Err(error) => self.context.emit(
                "error",
                json!({ "surface": self.props.borrow().surface.clone(), "message": error }),
            ),
        }
    }

    fn descriptor(&self) -> Option<super::host_api::SurfaceDescriptor> {
        self.mount
            .borrow()
            .as_ref()
            .map(|mount| mount.descriptor.clone())
    }

    fn start_load(&self) {
        let Some(descriptor) = self.descriptor() else {
            return;
        };
        let signature = format!(
            "{}:{}:{}:{}",
            descriptor.package_root.display(),
            descriptor.instance,
            descriptor.contribution,
            descriptor.revision
        );
        {
            let mut requested = self
                .requested
                .lock()
                .expect("plugin component request lock");
            if requested.as_deref() == Some(signature.as_str()) {
                return;
            }
            *requested = Some(signature.clone());
        }
        let package = installed_package_path(&descriptor.package_root);
        let trust = package_trust_root(&descriptor.package_root);
        let data_dir = package_data_dir(&descriptor.package_root)
            .unwrap_or_else(|| descriptor.package_root.clone());
        let requested = self.requested.clone();
        let context = self.context.clone();
        std::thread::spawn(move || {
            let result = load_verified(&package, &trust, &data_dir).and_then(|verified| {
                verified
                    .component
                    .ok_or_else(|| "plugin has no componentEntry".to_owned())
            });
            if requested
                .lock()
                .expect("plugin component request lock")
                .as_deref()
                != Some(signature.as_str())
            {
                return;
            }
            match result {
                Ok(chunk) => {
                    let id = chunk.id.clone();
                    context.chunk_loaded(chunk);
                    context.emit(
                        "ready",
                        json!({"surface": descriptor.surface, "chunkId": id}),
                    );
                }
                Err(error) => context.emit(
                    "error",
                    json!({ "surface": descriptor.surface, "message": error }),
                ),
            }
        });
    }

    fn release(&self) {
        if let Some(mount) = self.mount.borrow_mut().take() {
            host_api::detach(&mount.descriptor.surface);
        }
        *self
            .requested
            .lock()
            .expect("plugin component request lock") = None;
    }

    fn set_prop(&mut self, name: &str, value: Option<&JsonValue>) {
        if name == "surface" {
            self.props.borrow_mut().surface =
                value.and_then(JsonValue::as_str).unwrap_or_default().into();
        }
        self.sync();
    }

    fn command(&mut self, name: &str) {
        if name == "reload" {
            *self
                .requested
                .lock()
                .expect("plugin component request lock") = None;
            self.sync();
        }
    }
}

impl NativeComponent for PluginComponent {
    #[cfg(target_os = "linux")]
    fn widget(&self) -> platform::gtk4::Widget {
        self.widget.clone()
    }

    #[cfg(target_os = "macos")]
    fn view(&self) -> platform::objc2::rc::Retained<platform::objc2_app_kit::NSView> {
        self.view.clone()
    }

    fn set_prop(&mut self, name: &str, value: Option<&JsonValue>) {
        PluginComponent::set_prop(self, name, value);
    }

    fn command(&mut self, name: &str, _args: &JsonValue) {
        self.command(name);
    }
}

impl Drop for PluginComponent {
    fn drop(&mut self) {
        self.release();
    }
}
