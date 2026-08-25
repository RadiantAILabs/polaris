//! `DefaultPlugins` built through a full `finish()`.
//!
//! Lives in its own integration binary because `TracingPlugin::ready()`
//! installs the process-global tracing subscriber — only one server carrying
//! it can ever finish per process, and the lib test binary already spends
//! that slot.

use polaris_core_plugins::{
    Clock, DefaultPlugins, InspectionAPI, InspectionSinkRegistry, ServerInfo,
};
use polaris_system::plugin::PluginGroup;
use polaris_system::server::Server;

#[tokio::test]
async fn default_plugins_finishes_and_provides_the_default_surfaces() {
    let mut server = Server::new();
    // TracingPlugin's ready() decorates the model and tool registries.
    server.add_plugins(polaris_models::ModelsPlugin);
    server.add_plugins(polaris_tools::ToolsPlugin);
    // Under `--all-features`, `dashboard` makes `ModelsPlugin`/`ToolsPlugin`
    // require `AppPlugin`. Bind an ephemeral listener so `finish()` does not
    // claim a fixed port.
    #[cfg(feature = "dashboard")]
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        server.add_plugins(
            polaris_app::AppPlugin::new(polaris_app::AppConfig::new()).with_listener(listener),
        );
    }
    server.add_plugins(DefaultPlugins::new().build());
    server.finish().await.expect("DefaultPlugins must finish");

    let ctx = server.create_context();
    assert!(ctx.contains_resource::<ServerInfo>());
    assert!(ctx.contains_resource::<Clock>());
    assert!(
        server.api::<InspectionAPI>().is_some(),
        "the bundled InspectionPlugin must provide InspectionAPI"
    );
    assert!(
        server.contains_resource::<InspectionSinkRegistry>(),
        "the bundled InspectionPlugin must provide InspectionSinkRegistry"
    );
}
