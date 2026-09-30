use std::env;
use std::path::PathBuf;

use cmake::Config;

fn main() {
    let mut bindings = bindgen::Builder::default()
        .use_core()
        .default_enum_style(bindgen::EnumVariation::Rust {
            non_exhaustive: false,
        })
        .clang_arg("-DINPUTTINO_STATIC_DEFINE=1")
        .header("wrapper.h");

    let libdir_path = PathBuf::from("../../../")
        // Canonicalize the path as `rustc-link-search` requires an absolute
        // path.
        .canonicalize()
        .expect("cannot canonicalize path");

    // Rebuild the native backend when the local patch changes.
    for path in ["src", "include", "cmake", "CMakeLists.txt"] {
        println!("cargo:rerun-if-changed={}", libdir_path.join(path).display());
    }

    // Compile the library using CMake
    let dst = Config::new(libdir_path)
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("LIBINPUTTINO_INSTALL", "ON")
        .define("BUILD_TESTING", "OFF")
        .define("BUILD_SERVER", "OFF")
        .define("BUILD_C_BINDINGS", "ON")
        .profile("Release")
        .define("CMAKE_CONFIGURATION_TYPES", "Release")
        .build();

    println!("cargo:rustc-link-search=native={}/lib", dst.display());
    println!("cargo:rustc-link-search=native={}/lib64", dst.display());
    bindings = bindings.clang_arg(format!("-I{}/include/", dst.display()));

    // Dependencies
    println!("cargo:rustc-link-lib=evdev");
    println!("cargo:rustc-link-lib=stdc++");
    println!("cargo:rustc-link-lib=c++");
    println!("cargo:rustc-link-lib=static=libinputtino");

    let out = bindings.generate().expect("Unable to generate bindings");

    // Write the bindings to the $OUT_DIR/bindings.rs file.
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("bindings.rs");
    out.write_to_file(out_path)
        .expect("Couldn't write bindings!");
}
