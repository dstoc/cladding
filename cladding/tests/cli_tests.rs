use cladding::config::Config;
use cladding::network::resolve_network_settings;
use cladding::pods::render_pods_yaml;
use std::path::Path;

#[test]
fn render_pods_yaml_replaces_placeholders() {
    let settings = resolve_network_settings("demo", 1).unwrap();
    let config = Config {
        name: "demo".to_string(),
        sandbox_image: "sandbox:image".to_string(),
        cli_image: "cli:image".to_string(),
        mounts: Vec::new(),
    };
    let rendered = render_pods_yaml(Path::new("/tmp/project/.cladding"), &config, &settings);

    assert!(!rendered.contains("REPLACE_PROXY_POD_NAME"));
    assert!(!rendered.contains("REPLACE_CLI_IMAGE"));
    assert!(rendered.contains("demo-proxy-pod"));
    assert!(rendered.contains("sandbox:image"));
}
