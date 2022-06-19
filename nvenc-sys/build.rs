use bindgen::builder;

use std::env;
use std::path::PathBuf;

fn main() {
	println!("cargo:rerun-if-changed=nvEncodeAPI.h");
	println!("cargo:rustc-link-lib=dylib=nvidia-encode");

	// Generate bindings for NvFBC.h.
	let bindings = builder()
		.header("nvEncodeAPI.h")
		.parse_callbacks(Box::new(bindgen::CargoCallbacks))
		.generate()
		.expect("Unable to generate bindings");

	// Write the bindings to the $OUT_DIR/bindings.rs file.
	let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
	bindings
		.write_to_file(out_path.join("bindings.rs"))
		.expect("Couldn't write bindings!");
	}
