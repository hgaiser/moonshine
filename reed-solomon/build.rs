fn main() {
	cc::Build::new()
		.file("src/rs.c")
		.compile("rs");

	println!("cargo:rustc-link-lib=rs");
}
