# Memonotes

Memonotes is an early Linux/GNOME desktop app for local notes. The current GTK
window has a three-pane layout, folder creation/navigation, note search, pinning,
archive/restore, and moving notes between folders backed by SQLite. The editor
is currently plain text; WYSIWYG formatting, images, and Memos API
synchronization are not implemented yet.

## Fedora build prerequisites

Install the GTK development packages:

```sh
sudo dnf install gtk4-devel libadwaita-devel pkgconf-pkg-config
```

Make sure Cargo and npm are on `PATH`. Install the file watcher once:

```sh
cargo install cargo-watch
```

The root `package.json` is a shortcut list for Cargo commands; it does not add a
JavaScript runtime to the app. Run `npm run` to see all available commands.
Start commands from the project directory:

```sh
cd ~/repos/memonote
npm run dev
```

`npm run dev` opens the GTK app and rebuilds/restarts it when Rust source changes.
Use `Ctrl+C` in the terminal to stop the watcher.

| Command | Action |
| --- | --- |
| `npm run dev` | Watch Rust files, rebuild, and restart the GTK app on changes |
| `npm run watch:check` | Re-run the GTK compile check on changes without launching the app |
| `npm start` | Launch the app once |
| `npm run build` | Build an optimized release binary |
| `npm run check` | Compile-check the GTK app |
| `npm test` | Run local storage tests |
| `npm run format` | Format Rust sources |

The watcher restarts the app after source changes. Notes are stored separately,
so they remain on disk across restarts. The database lives under
`$XDG_DATA_HOME/memonotes`, or `~/.local/share/memonotes` when `XDG_DATA_HOME`
is unset.