use crate::commands::common::cargo_rustc_command;
use crate::error::*;
use crate::tools::*;
use crate::types::*;
use anyhow::format_err;
use cargo::{
    core::{
        compiler::{CompileKind, CompileMode, CompileTarget, Executor},
        manifest::TargetSourcePath,
        resolver::CliFeatures,
        shell::Verbosity,
        {PackageId, TargetKind, Workspace},
    },
    ops::CompileOptions,
    util::{CargoResult, Config as CargoConfig},
};
use cargo_util::ProcessBuilder;
use std::{
    ffi::{OsStr, OsString},
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};
use tempfile::Builder;

const NDK_GLUE_EXTRA_CODE: &'static str = r#"
#[no_mangle]
#[cfg(target_os = "android")]
unsafe extern "C" fn ANativeActivity_onCreate(
    activity: *mut std::os::raw::c_void,
    saved_state: *mut std::os::raw::c_void,
    saved_state_size: usize,
) {
    crossbow::ndk_glue::init(
        activity as _,
        saved_state as _,
        saved_state_size as _,
        main,
    );
}
"#;

const SOKOL_EXTRA_CODE: &'static str = r#"
mod cargo_apk_glue_code {
    extern "C" {
        pub fn sapp_ANativeActivity_onCreate(
            activity: *mut std::ffi::c_void,
            saved_state: *mut std::ffi::c_void,
            saved_state_size: usize,
        );
    }
    #[no_mangle]
    pub unsafe extern "C" fn ANativeActivity_onCreate(
        activity: *mut std::ffi::c_void,
        saved_state: *mut std::ffi::c_void,
        saved_state_size: usize,
    ) {
        sapp_ANativeActivity_onCreate(activity, saved_state, saved_state_size as _);
    }
    #[no_mangle]
    pub unsafe extern "C" fn sokol_main() {
        let _ = super::main();
    }
    #[link(name = "android")]
    #[link(name = "log")]
    #[link(name = "EGL")]
    #[link(name = "GLESv3")]
    extern "C" {}
"#;

/// Write a CMake toolchain which will remove references to the rustc build target before
/// including the NDK provided toolchain. The NDK provided android toolchain will set the
/// target appropriately Returns the path to the generated toolchain file
fn write_cmake_toolchain(
    min_sdk_version: u32,
    ndk_path: &Path,
    build_target_dir: &Path,
    build_target: AndroidTarget,
) -> CargoResult<PathBuf> {
    let toolchain_path = build_target_dir.join("cargo-apk.toolchain.cmake");
    let mut toolchain_file = fs::File::create(&toolchain_path).unwrap();
    writeln!(
        toolchain_file,
        r#"set(ANDROID_PLATFORM android-{min_sdk_version})
set(ANDROID_ABI {abi})
string(REPLACE "--target={build_target}" "" CMAKE_C_FLAGS "${{CMAKE_C_FLAGS}}")
string(REPLACE "--target={build_target}" "" CMAKE_CXX_FLAGS "${{CMAKE_CXX_FLAGS}}")
unset(CMAKE_C_COMPILER CACHE)
unset(CMAKE_CXX_COMPILER CACHE)
include("{ndk_path}/build/cmake/android.toolchain.cmake")"#,
        min_sdk_version = min_sdk_version,
        ndk_path = ndk_path.to_string_lossy().replace("\\", "/"), /* Use forward slashes even on
                                                                   * windows to avoid path
                                                                   * escaping issues. */
        build_target = build_target.rust_triple(),
        abi = build_target.android_abi(),
    )?;
    Ok(toolchain_path)
}

/// Returns path to NDK provided make
fn make_path(ndk_path: &Path) -> PathBuf {
    ndk_path.join("prebuild").join(HOST_TAG).join("make")
}

#[cfg(all(target_os = "windows", target_pointer_width = "64"))]
const HOST_TAG: &str = "windows-x86_64";

#[cfg(all(target_os = "windows", target_pointer_width = "32"))]
const HOST_TAG: &str = "windows";

#[cfg(target_os = "linux")]
const HOST_TAG: &str = "linux-x86_64";

#[cfg(target_os = "macos")]
const HOST_TAG: &str = "darwin-x86_64";

/// Generate source file that will be built
fn generate_lib_file(original_src_path: &Path, extra_code: Option<&str>) -> CargoResult<()> {
    // Determine the name of the temporary file
    let tmp_lib_file_prefix = format!(
        "__cargo_apk_{}",
        original_src_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(String::new)
    );

    // Create the temporary file
    let mut tmp_file = Builder::new()
        .prefix(&tmp_lib_file_prefix)
        .suffix(".tmp")
        .tempfile_in(original_src_path.parent().unwrap())?;

    let original_contents = fs::read_to_string(original_src_path).unwrap();
    writeln!(
        tmp_file,
        "{}\n{}",
        original_contents,
        extra_code.unwrap_or("")
    )?;
    Ok(())
}

fn exec_compilation(
    cmd: &ProcessBuilder,
    build_target_dir: &Path,
    build_target: AndroidTarget,
    ndk: &AndroidNdk,
    target_sdk_version: u32,
    nostrip: bool,
    profile: Profile,
    extra_code: Option<&str>,
    pre_compilation: Option<&mut dyn FnMut() -> CargoResult<()>>,
    target: &cargo::core::Target,
    mode: CompileMode,
    on_stdout_line: &mut dyn FnMut(&str) -> CargoResult<()>,
    on_stderr_line: &mut dyn FnMut(&str) -> CargoResult<()>,
) -> CargoResult<()> {
    if pre_compilation.is_some() {
        (pre_compilation.unwrap())()?;
    }

    if mode == CompileMode::Build
        && (target.kind() == &TargetKind::Bin || target.kind() == &TargetKind::ExampleBin)
    {
        let mut new_args = cmd.get_args().to_owned();

        // Determine source path
        let path = if let TargetSourcePath::Path(path) = target.src_path() {
            path.to_owned()
        } else {
            // Ignore other values
            return Ok(());
        };

        let original_src_filepath = path.canonicalize()?;

        generate_lib_file(&original_src_filepath, extra_code)?;

        // Create output directory inside the build target directory
        let build_path = build_target_dir.to_path_buf();
        fs::create_dir_all(&build_path).unwrap();

        // Change crate-type from bin to cdylib
        // Replace output directory with the directory we created
        let mut iter = new_args.iter_mut().rev().peekable();
        while let Some(arg) = iter.next() {
            if let Some(prev_arg) = iter.peek() {
                if *prev_arg == "--crate-type" && arg == "bin" {
                    *arg = "cdylib".into();
                } else if *prev_arg == "--out-dir" {
                    *arg = build_path.clone().into();
                }
            }
        }

        // Helper function to build arguments composed of concatenating two strings
        fn build_arg(start: &str, end: impl AsRef<OsStr>) -> OsString {
            let mut new_arg = OsString::new();
            new_arg.push(start);
            new_arg.push(end.as_ref());
            new_arg
        }

        // Determine paths
        let tool_root = &ndk.toolchain_dir().unwrap();
        let linker_path = tool_root
            .join("bin")
            .join(format!("{}-ld.gold", &build_target.ndk_triple()));
        let sysroot = tool_root.join("sysroot");
        let version_independent_libraries_path = sysroot
            .join("usr")
            .join("lib")
            .join(&build_target.ndk_triple());
        let version_specific_libraries_path =
            AndroidNdk::find_ndk_path(target_sdk_version, |platform| {
                version_independent_libraries_path.join(platform.to_string())
            })
            .map_err(|_| format_err!("unable to find NDK file"))?; // TODO: Fix this error
                                                                   // casting
        let gcc_lib_path = tool_root
            .join("lib/gcc")
            .join(&build_target.ndk_triple())
            .join("4.9.x");

        // Add linker arguments
        // Specify linker
        new_args.push(build_arg("-Clinker=", linker_path));

        // Set linker flavor
        new_args.push("-Clinker-flavor=ld".into());

        // Set system root
        new_args.push(build_arg("-Clink-arg=--sysroot=", sysroot));

        // Add version specific libraries directory to search path
        new_args.push(build_arg("-Clink-arg=-L", &version_specific_libraries_path));

        // Add version independent libraries directory to search path
        new_args.push(build_arg(
            "-Clink-arg=-L",
            &version_independent_libraries_path,
        ));

        // Add path to folder containing libgcc.a to search path
        new_args.push(build_arg("-Clink-arg=-L", gcc_lib_path));

        // Strip symbols for release builds
        if !nostrip && profile == Profile::Release {
            new_args.push("-Clink-arg=-strip-all".into());
        }

        // Require position independent code
        new_args.push("-Crelocation-model=pic".into());

        // Create new command
        let mut cmd = cmd.clone();
        cmd.args_replace(&new_args);

        // Execute the command
        cmd.exec_with_streaming(on_stdout_line, on_stderr_line, false)
            .map(drop)?;
    } else if mode == CompileMode::Test {
        // This occurs when --all-targets is specified
        eprintln!("Ignoring CompileMode::Test for target: {}", target.name());
    } else if mode == CompileMode::Build {
        let mut new_args = cmd.get_args().to_owned();

        // Change crate-type from cdylib to rlib
        let mut iter = new_args.iter_mut().rev().peekable();
        while let Some(arg) = iter.next() {
            if let Some(prev_arg) = iter.peek() {
                if *prev_arg == "--crate-type" && arg == "cdylib" {
                    *arg = "rlib".into();
                }
            }
        }

        let mut cmd = cmd.clone();
        cmd.args_replace(&new_args);
        cmd.exec_with_streaming(on_stdout_line, on_stderr_line, false)
            .map(drop)?
    } else {
        cmd.exec_with_streaming(on_stdout_line, on_stderr_line, false)
            .map(drop)?
    }

    Ok(())
}

/// Executor which builds binary and example targets as static libraries
struct MacroquadSharedLibraryExecutor {
    ndk: AndroidNdk,
    target_sdk_version: u32,

    build_target_dir: PathBuf,
    build_target: AndroidTarget,

    profile: Profile,
    nostrip: bool,
}

impl Executor for MacroquadSharedLibraryExecutor {
    fn exec(
        &self,
        cmd: &ProcessBuilder,
        _id: PackageId,
        target: &cargo::core::Target,
        mode: CompileMode,
        on_stdout_line: &mut dyn FnMut(&str) -> CargoResult<()>,
        on_stderr_line: &mut dyn FnMut(&str) -> CargoResult<()>,
    ) -> CargoResult<()> {
        let mut macroquad_pre_compilation = || -> CargoResult<()> {
            // Generate cmake toolchain and set environment variables to allow projects which use
            // the cmake crate to build correctly
            let cmake_toolchain_path = write_cmake_toolchain(
                self.target_sdk_version,
                self.ndk.ndk_path(),
                &self.build_target_dir,
                self.build_target,
            )?;
            std::env::set_var("CMAKE_TOOLCHAIN_FILE", cmake_toolchain_path);
            std::env::set_var("CMAKE_GENERATOR", r#"Unix Makefiles"#);
            std::env::set_var("CMAKE_MAKE_PROGRAM", make_path(self.ndk.ndk_path()));
            Ok(())
        };

        exec_compilation(
            cmd,
            &self.build_target_dir,
            self.build_target,
            &self.ndk,
            self.target_sdk_version,
            self.nostrip,
            self.profile,
            Some(SOKOL_EXTRA_CODE),
            Some(&mut macroquad_pre_compilation),
            target,
            mode,
            on_stdout_line,
            on_stderr_line,
        )
    }
}

struct BevySharedLibraryExecutor {
    ndk: AndroidNdk,
    target_sdk_version: u32,

    build_target_dir: PathBuf,
    build_target: AndroidTarget,

    profile: Profile,
    nostrip: bool,
}

impl Executor for BevySharedLibraryExecutor {
    fn exec(
        &self,
        cmd: &ProcessBuilder,
        _id: PackageId,
        target: &cargo::core::Target,
        mode: CompileMode,
        on_stdout_line: &mut dyn FnMut(&str) -> CargoResult<()>,
        on_stderr_line: &mut dyn FnMut(&str) -> CargoResult<()>,
    ) -> CargoResult<()> {
        exec_compilation(
            cmd,
            &self.build_target_dir,
            self.build_target,
            &self.ndk,
            self.target_sdk_version,
            self.nostrip,
            self.profile,
            Some(NDK_GLUE_EXTRA_CODE),
            None,
            target,
            mode,
            on_stdout_line,
            on_stderr_line,
        )
    }
}

pub fn compile_rust_for_android(
    ndk: &AndroidNdk,
    build_target: AndroidTarget,
    project_path: &Path,
    profile: Profile,
    features: Vec<String>,
    all_features: bool,
    no_default_features: bool,
    target_sdk_version: u32,
    lib_name: &str,
    engine: GameEngine,
) -> Result<()> {
    let cargo_config = CargoConfig::default()?;
    let workspace = Workspace::new(&project_path.join("Cargo.toml"), &cargo_config)?;
    let rust_triple = build_target.rust_triple();

    let build_target_dir = workspace
        .root()
        .join("target")
        .join(rust_triple)
        .join(profile);
    fs::create_dir_all(&build_target_dir).unwrap();

    // Set environment variables needed for use with the cc crate
    let (clang, clang_pp) = ndk.clang(build_target, target_sdk_version)?;
    let ar = ndk.toolchain_bin("ar", build_target)?;

    std::env::set_var(format!("CC_{}", rust_triple), &clang);
    std::env::set_var(format!("CXX_{}", rust_triple), &clang_pp);
    std::env::set_var(format!("AR_{}", rust_triple), &ar);

    // Use libc++. It is current default C++ runtime
    std::env::set_var("CXXSTDLIB", "c++");

    // Configure compilation options so that we will build the desired build_target
    let config = workspace.config();

    // Avoid too much log info
    config.shell().set_verbosity(Verbosity::Normal);

    let mut opts = CompileOptions::new(config, CompileMode::Build)?;

    // Set the compilation target
    opts.build_config.requested_kinds = vec![CompileKind::Target(CompileTarget::new(rust_triple)?)];

    // Set features options
    opts.cli_features =
        CliFeatures::from_command_line(&features, all_features, no_default_features)?;

    // Set the path and file name for the generated shared library
    opts.target_rustc_args = Some(vec![format!(
        "--emit=link={}",
        build_target_dir
            .join(lib_name)
            .into_os_string()
            .into_string()
            .unwrap()
    )]);

    // Set desired profile
    if profile == Profile::Release {
        opts.build_config.requested_profile = "release".into();
    }

    // Create the executor
    let executor: Arc<dyn Executor> = match engine {
        GameEngine::Bevy => Arc::new(BevySharedLibraryExecutor {
            ndk: ndk.clone(),
            target_sdk_version,
            profile,
            build_target_dir,
            build_target,
            nostrip: false,
        }),
        GameEngine::Macroquad => Arc::new(MacroquadSharedLibraryExecutor {
            ndk: ndk.clone(),
            target_sdk_version,
            profile,
            build_target_dir,
            build_target,
            nostrip: false,
        }),
    };

    // Compile all targets for the requested build target
    cargo::ops::compile_with_exec(&workspace, &opts, &executor)?;
    Ok(())
}

/// Compiles rust code for android
pub fn compile_rust_for_android_(
    ndk: &AndroidNdk,
    target: Target,
    build_target: AndroidTarget,
    project_path: &Path,
    profile: Profile,
    features: Vec<String>,
    all_features: bool,
    no_default_features: bool,
    target_sdk_version: u32,
) -> Result<()> {
    let crate_types = vec![CrateType::Cdylib];
    let mut cargo = cargo_rustc_command(
        &target,
        project_path,
        &profile,
        &features,
        all_features,
        no_default_features,
        &build_target.into(),
        &crate_types,
    );
    let triple = build_target.rust_triple();
    // Takes clang and clang_pp paths
    let (clang, clang_pp) = ndk.clang(build_target, target_sdk_version)?;
    cargo.env(format!("CC_{}", triple), &clang);
    cargo.env(format!("CXX_{}", triple), &clang_pp);
    cargo.env(cargo_env_target_cfg("LINKER", triple), &clang);
    let ar = ndk.toolchain_bin("ar", build_target)?;
    cargo.env(format!("AR_{}", triple), &ar);
    cargo.env(cargo_env_target_cfg("AR", triple), &ar);
    cargo.output_err(true)?;
    Ok(())
}

fn cargo_env_target_cfg(tool: &str, target: &str) -> String {
    let utarget = target.replace("-", "_");
    let env = format!("CARGO_TARGET_{}_{}", &utarget, tool);
    env.to_uppercase()
}
