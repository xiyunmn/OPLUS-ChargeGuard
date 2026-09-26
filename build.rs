use std::{collections::BTreeMap, env, fs};
fn main() {
    println!("cargo:rerun-if-changed=module/module.prop");
    let text = fs::read_to_string("module/module.prop").expect("module metadata");
    let meta = text
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        meta["version"],
        env::var("CARGO_PKG_VERSION").unwrap(),
        "module/Cargo version mismatch"
    );
    for (key, var) in [
        ("id", "CG_ID"),
        ("name", "CG_NAME"),
        ("author", "CG_AUTHOR"),
    ] {
        let value = meta[key];
        assert!(!value.contains(['\n', '\r']));
        println!("cargo:rustc-env={var}={value}");
    }
}
