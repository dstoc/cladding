use cladding::assets::render_pods_yaml;
use cladding::network::resolve_network_settings;
use std::path::Path;

#[test]
fn render_pods_yaml_replaces_placeholders() {
    let settings = resolve_network_settings("demo", "10.90.1.0/24").unwrap();
    let rendered = render_pods_yaml(
        Path::new("/tmp/project/.cladding"),
        "sandbox:image",
        "cli:image",
        &settings.proxy_pod_name,
        &settings.sandbox_pod_name,
        &settings.cli_pod_name,
        &settings.proxy_ip,
        &settings.sandbox_ip,
        &settings.cli_ip,
    );

    assert!(!rendered.contains("REPLACE_PROXY_POD_NAME"));
    assert!(!rendered.contains("REPLACE_CLI_IMAGE"));
    assert!(rendered.contains("demo-proxy-pod"));
    assert!(rendered.contains("sandbox:image"));
}
