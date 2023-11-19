use std::path::{PathBuf, Path};

use clap::Parser;

#[derive(Parser)]
struct Args {
	/// Path to a 'libraryfolders.vdf' file.
	///
	/// Can be provided multiple times, in which case the libraries will be inspected one by one.
	libraries: Vec<PathBuf>,

	/// If provided, run this command before starting this application.
	///
	/// Note that multiple entries be provided, in which case they will be executed in that same order.
    #[arg(short, long)]
	run_before: Option<Vec<String>>,

	/// If provided, run this command after stopping this application.
	///
	/// Note that multiple entries be provided, in which case they will be executed in that same order.
	#[arg(short, long)]
	run_after: Option<Vec<String>>,
}

fn main() -> Result<(), ()> {
	let args = Args::parse();

	for library_path in args.libraries {
		let library = std::fs::read_to_string(&library_path)
			.map_err(|e| eprintln!("Failed to open library: {e}"))?;

		// Poor man's library parsing.
		let start_apps = library.find("apps")
			.ok_or_else(|| eprintln!("Failed to find 'apps' key in {library_path:?}."))?;
		let library = &library[start_apps..];
		let stop_apps = library.find('}')
			.ok_or_else(|| eprintln!("Failed to find end of 'apps' section."))?;
		let library = &library[..stop_apps];

		for line in library.lines().skip(2) {
			if line.trim().is_empty() {
				continue;
			}

			let game_id = match line.split('\"').nth(1) {
				Some(game_id) => game_id,
				None => {
					eprintln!("Failed to parse library entry: '{line}'");
					continue;
				},
			};

			let game_id: u32 = match game_id.parse() {
				Ok(game_id) => game_id,
				Err(e) => {
					eprintln!("Failed to parse game id: {e}");
					continue;
				},
			};

			let game_name = get_game_name(game_id, &library_path)?;

			// Skip things that aren't really games.
			if game_name.starts_with("Proton")
				|| game_name.starts_with("Steam Linux Runtime")
				|| game_name.starts_with("Steamworks Common Redistributables") {
				continue;
			}

			println!("[[applications]]");
			println!("title = \"{}\"", get_game_name(game_id, &library_path)?);

			if let Some(run_before) = &args.run_before {
				println!("run_before = [");
				for command in run_before {
					let command = command.replace("{game_id}", &game_id.to_string());
					println!("	[{command}],");
				}
				println!("]");
			}
			if let Some(run_after) = &args.run_after {
				println!("run_after = [");
				for command in run_after {
					let command = command.replace("{game_id}", &game_id.to_string());
					println!("	[{command}],");
				}
				println!("]");
			}

			let boxart = library_path
				.parent()
				.ok_or_else(|| eprintln!("Expected at least two parents, found none for '{library_path:?}"))?
				.parent()
				.ok_or_else(|| eprintln!("Expected at least two parents, found one for '{library_path:?}"))?
				.join(format!("appcache/librarycache/{game_id}_library_600x900.jpg"));
			if boxart.exists() {
				println!("boxart = {boxart:?}");
			} else {
				eprintln!("No boxart for game '{game_name}' at '{boxart:?}");
			}
			println!();
		}
	}

	Ok(())
}

fn get_game_name(game_id: u32, library: &Path) -> Result<String, ()> {
	let manifest_path = library
		.parent()
		.ok_or_else(|| eprintln!("Expected '{library:?}' to have a parent, but couldn't find one."))?
		.join(format!("appmanifest_{}.acf", game_id));
	let manifest = std::fs::read_to_string(manifest_path)
		.map_err(|e| eprintln!("Failed to open manifest: {e}"))?;
	let name_line = manifest
		.lines()
		.find(|l| l.contains("\"name\""));

	match name_line {
		Some(line) => {
			line
				.split('\"')
				.nth(3)
				.ok_or_else(|| eprintln!("Line '{}' doesn't match expected format (expected: \"name\" \"<NAME>\").", line))
				.map(|l| l.to_string())
		},
		None => {
			eprintln!("Couldn't find name for game with ID '{game_id}'.");
			Err(())
		},
	}
}