use crossbundle_tools::{
    commands::{android::*, gen_minimal_macroquad_project},
    tools::{AndroidNdk, AndroidSdk},
    types::*,
};

#[test]
fn test_compile_android() {
    let tempdir = tempfile::tempdir().unwrap();
    let dir = tempdir.path();
    let _name = gen_minimal_macroquad_project(dir).unwrap();

    let sdk = AndroidSdk::from_env().unwrap();
    let ndk = AndroidNdk::from_env(Some(sdk.sdk_path())).unwrap();
    compile_macroquad_rust_for_android(
        &ndk,
        // Target::Lib,
        AndroidTarget::Aarch64LinuxAndroid,
        dir,
        Profile::Release,
        vec![],
        false,
        false,
        30,
        "macroquad_test_project",
    )
    .unwrap();
}
