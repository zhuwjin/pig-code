use std::sync::Arc;

use nsh_platform_linux::{
    AppOptions, AppView, AssetBase, ComponentContext, Components, NativeComponent, gtk4 as gtk,
};
use serde_json::Value as JsonValue;

use super::{COMPONENT_NAME, Props};
use crate::plugin_runtime::{configure_host, load_verified, send_metrics};

pub fn register(components: &Components) {
    components.register(
        COMPONENT_NAME,
        std::rc::Rc::new(|context| Box::new(PluginSurface::new(context))),
    );
}

struct PluginSurface {
    view: std::rc::Rc<AppView>,
    host: nsh_engine::HostChannel,
    context: ComponentContext,
    props: Props,
    loaded: bool,
    failed: bool,
}

impl PluginSurface {
    fn new(context: ComponentContext) -> Self {
        let host = nsh_engine::HostChannel::new();
        let options = AppOptions {
            host: host.clone(),
            data_dir: Some(pig_utils::data_dir().join("nsh")),
            ..AppOptions::default()
        };
        let view = AppView::new(options);
        Self {
            view,
            host,
            context,
            props: Props::default(),
            loaded: false,
            failed: false,
        }
    }

    fn load(&mut self) {
        if self.loaded || self.failed {
            return;
        }
        let Some((package, root, slot)) = self.props.ready() else {
            return;
        };
        match load_verified(package, root, &pig_utils::data_dir()) {
            Ok(plugin) => {
                configure_host(
                    &self.host,
                    slot,
                    Arc::new({
                        let context = self.context.clone();
                        move |event, data| context.emit(event, data)
                    }),
                );
                self.view
                    .set_asset_base(Some(AssetBase::Directory(plugin.assets)));
                if let Some(theme) = self.props.theme()
                    && let Err(error) = self.view.set_theme_overrides(Some(theme))
                {
                    self.context.emit(
                        "pluginError",
                        serde_json::json!({ "message": error.to_string() }),
                    );
                }
                self.view.load(plugin.bundle.into());
                self.loaded = true;
            }
            Err(error) => {
                self.failed = true;
                self.context
                    .emit("pluginError", serde_json::json!({ "message": error }));
            }
        }
    }
}

impl NativeComponent for PluginSurface {
    fn widget(&self) -> gtk::Widget {
        self.view.widget()
    }

    fn set_prop(&mut self, name: &str, value: Option<&JsonValue>) {
        match name {
            "theme" => {
                if let Some(theme) = value.and_then(|value| value.as_str())
                    && let Err(error) = self.view.set_theme_overrides(Some(theme))
                {
                    self.context.emit(
                        "pluginError",
                        serde_json::json!({ "message": error.to_string() }),
                    );
                }
                self.props.set(name, value);
            }
            "metrics" => send_metrics(&self.host, value.unwrap_or(&JsonValue::Null)),
            _ => {
                self.props.set(name, value);
                self.load();
            }
        }
    }
}

impl Drop for PluginSurface {
    fn drop(&mut self) {
        self.view.stop();
    }
}
