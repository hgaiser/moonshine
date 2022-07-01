use std::collections::HashSet;
use std::env;
use std::path::PathBuf;

#[derive(Debug)]
struct IgnoreMacros(HashSet<String>);

impl bindgen::callbacks::ParseCallbacks for IgnoreMacros {
	fn will_parse_macro(&self, name: &str) -> bindgen::callbacks::MacroParsingBehavior {
		if self.0.contains(name) {
			bindgen::callbacks::MacroParsingBehavior::Ignore
		} else {
			bindgen::callbacks::MacroParsingBehavior::Default
		}
	}
}

fn main() {
	println!("cargo:rerun-if-changed=wrapper.h");

	// https://github.com/rust-lang/rust-bindgen/issues/687
	let ignored_macros = IgnoreMacros(
		vec![
			"FP_INFINITE".into(),
			"FP_NAN".into(),
			"FP_NORMAL".into(),
			"FP_SUBNORMAL".into(),
			"FP_ZERO".into(),
			"IPPORT_RESERVED".into(),
		]
		.into_iter()
		.collect(),
	);

	let libraries = [
		pkg_config::Config::new()
			.atleast_version("57.17.100")
			.probe("libavutil")
			.unwrap(),
		pkg_config::Config::new()
			.atleast_version("59.18.100")
			.probe("libavcodec")
			.unwrap(),
		pkg_config::Config::new()
			.atleast_version("59.16.100")
			.probe("libavformat")
			.unwrap(),
		pkg_config::Config::new()
			.atleast_version("11.0")
			.probe("cuda")
			.unwrap(),
	];

	println!("libraries: {:#?}", libraries);

	let mut wrapper_builder = cc::Build::new();
	println!("cargo:rustc-link-lib=static=moonshine_ffmpeg");

	let mut bindings = bindgen::Builder::default()
		.header("wrapper.h")
		.parse_callbacks(Box::new(ignored_macros))
		.rustfmt_bindings(true)
	;

	for library in &libraries {
		for include_path in &library.include_paths {
			bindings = bindings
				.clang_arg(format!("-I{}", include_path.to_str().unwrap()));
		}
		wrapper_builder.includes(&library.include_paths);

		for lib_path in &library.link_paths {
			bindings = bindings
				.clang_arg(format!("-L{}", lib_path.to_str().unwrap()));
		}

		for lib in &library.libs {
			println!("cargo:rustc-link-lib=dylib={}", lib);
		}
	}

	wrapper_builder
		.file("wrapper.c")
		.compile("moonshine_ffmpeg");

	let bindings = bindings
		.generate()
		.expect("Unable to generate bindings");

	let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
	bindings
		.write_to_file(out_path.join("bindings.rs"))
		.expect("Couldn't write bindings!");
	}
