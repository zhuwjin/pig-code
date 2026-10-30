use nsh_platform_windows::{
    AppOptions, AppSource, AssetBase, Bundle, Components, run_app, set_ui_sets,
};

pub fn run() {
    let components = Components::default();
    super::resize_guard::register(&components);
    nsh_app_pig_code_ts::plugin_surface::windows::register(&components);
    let host = nsh_engine::HostChannel::new();
    host.set_params(super::params());
    let options = AppOptions {
        app_id: "com.example.pigcodets".into(),
        title: "Pig Code".into(),
        components,
        host,
        asset_base: Some(AssetBase::Directory(super::asset_base())),
        data_dir: Some(pig_utils::data_dir().join("nsh")),
        ..AppOptions::default()
    };
    set_ui_sets(vec![nsh_base::registry(), nsh_app_pig_code_ts::registry()]);
    let bundle = Bundle::read(&super::bundle_path()).expect("read app bundle");
    let _ = run_app(AppSource::Bundle(bundle), options);
}
