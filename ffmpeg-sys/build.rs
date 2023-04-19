fn main() {
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
			.atleast_version("7.1.100")
			.probe("libavformat")
			.unwrap(),
		pkg_config::Config::new()
			.probe("libswscale")
			.unwrap(),
		pkg_config::Config::new()
			.atleast_version("11.0")
			.probe("cuda")
			.unwrap(),
	];

	for library in &libraries {
		for lib in &library.libs {
			println!("cargo:rustc-link-lib=dylib={lib}");
		}
	}
}
