#[tokio::main]
async fn main() -> std::io::Result<()> {
    use css_loader_backend::rpc;

    decky_plugin::Plugin::new()
        .method("fetch_theme_path", |c, a| rpc(c, a, "fetch_theme_path"))
        .method("get_last_load_errors", |c, a| {
            rpc(c, a, "get_last_load_errors")
        })
        .method("get_themes", |c, a| rpc(c, a, "get_themes"))
        .method("reset", |c, a| rpc(c, a, "reset"))
        .method("enable_server", |c, a| rpc(c, a, "enable_server"))
        .method("get_server_state", |c, a| rpc(c, a, "get_server_state"))
        .method("get_watch_state", |c, a| rpc(c, a, "get_watch_state"))
        .method("get_backend_version", |c, a| {
            rpc(c, a, "get_backend_version")
        })
        .method("dummy_function", |c, a| rpc(c, a, "dummy_function"))
        .method("fetch_class_mappings", |c, a| {
            rpc(c, a, "fetch_class_mappings")
        })
        .method("is_standalone", |c, a| rpc(c, a, "is_standalone"))
        .method("set_theme_state", |c, a| rpc(c, a, "set_theme_state"))
        .method("set_patch_of_theme", |c, a| rpc(c, a, "set_patch_of_theme"))
        .method("set_component_of_theme_patch", |c, a| {
            rpc(c, a, "set_component_of_theme_patch")
        })
        .method("download_theme_from_url", |c, a| {
            rpc(c, a, "download_theme_from_url")
        })
        .method("delete_theme", |c, a| rpc(c, a, "delete_theme"))
        .method("store_read", |c, a| rpc(c, a, "store_read"))
        .method("store_write", |c, a| rpc(c, a, "store_write"))
        .method("toggle_watch_state", |c, a| rpc(c, a, "toggle_watch_state"))
        .method("generate_preset_theme", |c, a| {
            rpc(c, a, "generate_preset_theme")
        })
        .method("generate_preset_theme_from_theme_names", |c, a| {
            rpc(c, a, "generate_preset_theme_from_theme_names")
        })
        .method("upload_theme", |c, a| rpc(c, a, "upload_theme"))
        .on_main(|context| async move { css_loader_backend::run(context).await })
        .run()
        .await
}
