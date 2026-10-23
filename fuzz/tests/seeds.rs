use std::path::Path;

use diskgc_core::{config, profile};

#[test]
fn yaml_seeds_reach_successful_loads() {
    let profiles = profile::parse_profiles(include_str!("../seeds/yaml_loader/profiles"))
        .expect("profile seed must parse");
    profile::Library::with_user_profiles(&profiles).expect("profile seed must load");
    config::parse(
        Path::new("/fuzz/config.yaml"),
        include_bytes!("../seeds/yaml_loader/config"),
    )
    .expect("config seed must load");
}
