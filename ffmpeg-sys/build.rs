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
	println!("cargo:rerun-if-changed=wrapper.c");

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

	let mut bindings = bindgen::Builder::default()
		.header("wrapper.h")
		.parse_callbacks(Box::new(ignored_macros))
		.rustfmt_bindings(true)
		.allowlist_function("cu.*")
		.allowlist_type("CU.*")
		.allowlist_function("av.*")
		.allowlist_var("AV.*")
		.allowlist_var("EAGAIN")
		.allowlist_type("AV.*")
	;

	for library in &libraries {
		for include_path in &library.include_paths {
			bindings = bindings
				.clang_arg(format!("-I{}", include_path.to_str().unwrap()));
		}

		for lib_path in &library.link_paths {
			bindings = bindings
				.clang_arg(format!("-L{}", lib_path.to_str().unwrap()));
		}

		for lib in &library.libs {
			println!("cargo:rustc-link-lib=dylib={}", lib);
		}
	}

	let bindings = bindings
		.generate()
		.expect("Unable to generate bindings");

	let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
	bindings
		.write_to_file(out_path.join("bindings.rs"))
		.expect("Couldn't write bindings!");
	}
