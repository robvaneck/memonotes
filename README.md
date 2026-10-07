# Memonotes

Memonotes is an early Linux/GNOME desktop app for local notes. The current GTK
window has a three-pane layout, folder creation/navigation, note search, pinning,
archive/restore, moving notes between folders, and pastel note colors backed by
SQLite. Note colors adapt to light and dark appearance. Memos accounts can be
added in Settings; account details are stored in SQLite and PATs in GNOME
Keyring. The editor is currently plain text; WYSIWYG formatting, images, and
Memos note synchronization are not implemented yet.

## Fedora build prerequisites

Install the GTK development packages:

```sh
sudo dnf install gtk4-devel libadwaita-devel libsecret-devel pkgconf-pkg-config
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
Changes are debounced for one second. The app only starts after a successful build.
After a compile error or app exit (including exit status 0), the watcher stays
active and retries on the next file change. A compile error still needs to be
fixed; it does not stop the watcher. Dev instances skip the shared D-Bus
application registration to avoid registration races during restarts.
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

## Memos API probe

The API probe checks a Memos Personal Access Token against
`GET /api/v1/auth/me`, then reads all active memos and accessible spaces,
following pagination. It prints counts only; note contents are not printed or
saved by the probe. Provide the server URL and token through the environment:

```sh
export MEMONOTES_MEMOS_URL='https://memos.example.com'
read -rsp 'Memos PAT: ' MEMONOTES_MEMOS_TOKEN
printf '\n'
export MEMONOTES_MEMOS_TOKEN
cargo run --bin memos-api-probe
unset MEMONOTES_MEMOS_TOKEN
```

For a local HTTP server, also set `MEMONOTES_ALLOW_INSECURE_HTTP=1` explicitly.
HTTP does not encrypt the token in transit.

The watcher restarts the app after source changes. Notes are stored separately,
so they remain on disk across restarts. The database lives under
`$XDG_DATA_HOME/memonotes`, or `~/.local/share/memonotes` when `XDG_DATA_HOME`
is unset.