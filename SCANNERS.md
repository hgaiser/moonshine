# Application scanners

Scanners find installed applications, so you don't have to add them one by one.
Add one `[[application_scanner]]` table per scanner to `config.toml`, or add them on the Applications page of the control panel.
See the [README](README.md#configuration) for the rest of the configuration.

- [Steam](#steam)
- [Desktop entries](#desktop-entries)
- [Lutris](#lutris)
- [Heroic](#heroic)
- [Bottles](#bottles)
- [Faugus](#faugus)

Commands are resolved against Moonshine's own `PATH`.
Under the packaged systemd service, this is systemd's default `PATH`, not the `PATH` of your login shell.
A bare command name works for packages that install into `/usr/bin`.
For an AppImage or anything under `~/.local/bin`, use an absolute path.

## Steam

Finds all installed Steam games. The default configuration includes this scanner.

```toml
[[application_scanner]]
type = "steam"
library = "$HOME/.local/share/Steam"
command = ["/usr/bin/steam", "-bigpicture", "steam://rungameid/{game_id}"]
```

## Desktop entries

Finds applications from `.desktop` files:

```toml
[[application_scanner]]
type = "desktop"
directories = [
  "$HOME/.local/share/applications",
  "/usr/share/applications",
]
include_terminal = false
resolve_icons = true
```

## Lutris

Finds all installed Lutris games:

```toml
[[application_scanner]]
type = "lutris"
command = ["/usr/bin/lutris", "lutris:rungame/{slug}"]
```

- `{slug}` is the Lutris slug of the game.
- Box art comes from the `coverart/` directory of Lutris, when available.
- The default database is `~/.local/share/lutris/pga.db`. Set `pga_db` to use a different one.

## Heroic

Finds all installed [Heroic Games Launcher](https://github.com/Heroic-Games-Launcher/HeroicGamesLauncher) games (Epic, GOG, Amazon and manually added games):

```toml
[[application_scanner]]
type = "heroic"
command = ["heroic", "--no-gui", "heroic://launch?appName={app_name}&runner={runner}"]
```

If you installed Heroic from Flathub, launch it through Flatpak:

```toml
[[application_scanner]]
type = "heroic"
command = [
  "dbus-run-session", "--",
  "flatpak", "run", "com.heroicgameslauncher.hgl",
  "--no-gui", "heroic://launch?appName={app_name}&runner={runner}",
]
```

The `dbus-run-session` prefix keeps the game inside Moonshine's compositor, not on your desktop.
See [TIPS.md](TIPS.md#run-flatpak-steam-inside-moonshines-compositor) for why Flatpak needs it.

- `{app_name}` is the internal game id of Heroic.
- `{runner}` is the store of the game: `legendary` (Epic), `gog` (GOG), `nile` (Amazon) or `sideload` (manually added).
- Box art comes from the `images-cache/` directory of Heroic, for art that Heroic has already downloaded.
- The default configuration directory is `~/.config/heroic`. If only the Flatpak is installed, it is `~/.var/app/com.heroicgameslauncher.hgl/config/heroic`. Set `config_dir` to use a different one.

## Bottles

Finds the games and applications in the Library tab of [Bottles](https://github.com/bottlesdevs/Bottles):

```toml
[[application_scanner]]
type = "bottles"
command = ["bottles-cli", "run", "-b", "{bottle}", "--program-id", "{program_id}"]
umu_command = ["bottles-cli", "umu", "run", "--game", "{umu_game}"]
```

Only the entries that you added to the Bottles Library ("Add to Library" on a program) are shown, not every program that Bottles finds.
UMU entries show when Bottles reports them as ready.
Steam shortcuts in the Library are skipped.
`umu_command` is optional. Without it, UMU entries are skipped.

> [!NOTE]
> Bottles starts executables outside the bottle (for example on a separate game drive) through `wine start`, which exits immediately. Moonshine then ends the session shortly after launch. Games installed inside the bottle prefix are not affected.

- `{bottle}` is the name of the bottle, `{program_id}` the internal id of the program, `{name}` the display name of the entry, and `{umu_game}` the id of a game that the Bottles UMU integration manages.
- A bare `bottles-cli` only works for a native Bottles. For Flatpak Bottles, see [TIPS.md](TIPS.md#run-flatpak-bottles-inside-moonshines-compositor).
- Box art comes from the Bottles grid images, when available. Otherwise the icon of the program is used.
- The default data directory is the first of `~/.local/share/bottles` and `~/.var/app/com.usebottles.bottles/data/bottles` that contains a `library.yml`. So old data from an uninstalled native Bottles does not hide the Flatpak install. Set `data_dir` to use a different one.

## Faugus

Finds the games in your [Faugus Launcher](https://github.com/Faugus/faugus-launcher) library:

```toml
[[application_scanner]]
type = "faugus"
command = ["faugus-launcher", "--game", "{game_id}"]
```

If you installed Faugus from Flathub, launch it through Flatpak, with the same `dbus-run-session` prefix as for Heroic:

```toml
[[application_scanner]]
type = "faugus"
command = [
  "dbus-run-session", "--",
  "flatpak", "run", "--command=/app/bin/faugus-launcher", "io.github.Faugus.faugus-launcher",
  "--game", "{game_id}",
]
```

- `{game_id}` is the Faugus id of the game.
- Each game starts in the folder of its executable (its `working_directory`), as it does from the Faugus window. `faugus-launcher --game` alone keeps the directory of the caller, and some games quit immediately when they start in a different directory.
- Hidden games are skipped, unless you set `include_hidden = true`.
- Games that use the Faugus Steam runner are skipped. That runner gives the game to Steam and exits shortly after. The Steam scanner launches those games.
- Box art comes from the cover of the game in Faugus, when available. Otherwise the banner is used, and then the icon.
- The default data directory is the first that contains a `games.json` of: `~/.local/share/faugus-launcher`, `~/.var/app/io.github.Faugus.faugus-launcher/data/faugus-launcher`, `~/.config/faugus-launcher` and `~/.var/app/io.github.Faugus.faugus-launcher/config/faugus-launcher`. Older Faugus releases kept their library in the `config` directories. Set `data_dir` to use a different one.
