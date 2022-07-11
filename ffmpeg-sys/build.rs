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

	println!("libraries: {:#?}", libraries);

	let mut bindings = bindgen::Builder::default()
		.header("wrapper.h")
		.parse_callbacks(Box::new(ignored_macros))
		.rustfmt_bindings(true)
		// Blacklist functions with u128 in signature.
		// https://github.com/zmwangx/rust-ffmpeg-sys/issues/1
		// https://github.com/rust-lang/rust-bindgen/issues/1549
		.blocklist_function("acoshl")
		.blocklist_function("acosl")
		.blocklist_function("asinhl")
		.blocklist_function("asinl")
		.blocklist_function("atan2l")
		.blocklist_function("atanhl")
		.blocklist_function("atanl")
		.blocklist_function("cbrtl")
		.blocklist_function("ceill")
		.blocklist_function("copysignl")
		.blocklist_function("coshl")
		.blocklist_function("cosl")
		.blocklist_function("dreml")
		.blocklist_function("ecvt_r")
		.blocklist_function("erfcl")
		.blocklist_function("erfl")
		.blocklist_function("exp2l")
		.blocklist_function("expl")
		.blocklist_function("expm1l")
		.blocklist_function("fabsl")
		.blocklist_function("fcvt_r")
		.blocklist_function("fdiml")
		.blocklist_function("finitel")
		.blocklist_function("floorl")
		.blocklist_function("fmal")
		.blocklist_function("fmaxl")
		.blocklist_function("fminl")
		.blocklist_function("fmodl")
		.blocklist_function("frexpl")
		.blocklist_function("gammal")
		.blocklist_function("hypotl")
		.blocklist_function("ilogbl")
		.blocklist_function("isinfl")
		.blocklist_function("isnanl")
		.blocklist_function("j0l")
		.blocklist_function("j1l")
		.blocklist_function("jnl")
		.blocklist_function("ldexpl")
		.blocklist_function("lgammal")
		.blocklist_function("lgammal_r")
		.blocklist_function("llrintl")
		.blocklist_function("llroundl")
		.blocklist_function("log10l")
		.blocklist_function("log1pl")
		.blocklist_function("log2l")
		.blocklist_function("logbl")
		.blocklist_function("logl")
		.blocklist_function("lrintl")
		.blocklist_function("lroundl")
		.blocklist_function("modfl")
		.blocklist_function("nanl")
		.blocklist_function("nearbyintl")
		.blocklist_function("nextafterl")
		.blocklist_function("nexttoward")
		.blocklist_function("nexttowardf")
		.blocklist_function("nexttowardl")
		.blocklist_function("powl")
		.blocklist_function("qecvt")
		.blocklist_function("qecvt_r")
		.blocklist_function("qfcvt")
		.blocklist_function("qfcvt_r")
		.blocklist_function("qgcvt")
		.blocklist_function("remainderl")
		.blocklist_function("remquol")
		.blocklist_function("rintl")
		.blocklist_function("roundl")
		.blocklist_function("scalbl")
		.blocklist_function("scalblnl")
		.blocklist_function("scalbnl")
		.blocklist_function("significandl")
		.blocklist_function("sinhl")
		.blocklist_function("sinl")
		.blocklist_function("sqrtl")
		.blocklist_function("strtold")
		.blocklist_function("tanhl")
		.blocklist_function("tanl")
		.blocklist_function("tgammal")
		.blocklist_function("truncl")
		.blocklist_function("y0l")
		.blocklist_function("y1l")
		.blocklist_function("ynl")
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
