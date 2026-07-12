# Sessions

A session records `{directory: [commands]}` pairs for later replay. Loading a session starts new processes; it does not restore the processes that existed when the file was saved. The [daemon](README.md#directory--environment-configuration) separately keeps live jobs running across client disconnects.

## Storage

Sessions live under the [config directory](README.md#config-directory-sessions), one file per session:

```text
<config>/fleetcom/sessions/<name>.json
```

`<config>` is `$FLEETCOM_CONFIG_DIR` if set, otherwise the platform config directory (`~/.config/fleetcom` on Linux, `~/Library/Application Support/fleetcom` on macOS). It's created on the first save.

The filename derives from the session name. Leading and trailing whitespace is removed; control characters and any of `* " / \ < > : | ? .` become `_`; the result is capped at 255 characters. As a result, `my/session` becomes `my_session.json` and `a.b` becomes `a_b.json`. Replacing `.` prevents the name from supplying another extension.

## Format

A session is a JSON object mapping a working directory to the commands to run there:

```json
{
  "/home/you/work/api": [
    "cargo watch -x test",
    "cargo run"
  ],
  "/tmp": [
    "top"
  ]
}
```

- Keys are directory paths: each task's working directory.
- Values are ordered lists of shell command strings. Order is preserved, and each command runs in its own PTY under that directory.
- Directories serialize alphabetically. Command order remains stable within each directory.

The schema is a flat map with no version field or metadata, so files can be edited directly. Invalid JSON fails the load. Within valid JSON, entries that do not produce string command values are omitted.

## Saving and loading

- Save: `w` in the dashboard, type a name, `Enter`. Writes the current task set (each task's directory and command) to `<name>.json`.
- Load in-app: `o`, pick from the list, `Enter`.
- Load at launch: `fleetcom <name>`.

Loading always spawns new processes from the stored commands. Existing live jobs are daemon state and are not part of the session file.
