# Sessions

A session records commands, working directories, and each task's optional group assignment and display name for repeatable launches. Loading starts new processes; it does not restore the processes that existed when the file was saved. Live process continuity belongs to the [daemon](README.md#directory--environment-configuration), which keeps jobs running across client disconnects.

## Storage

Fleetcom stores one JSON file per session. The session directory resolves in this order:

| Condition | Session directory |
| -- | -- |
| `FLEETCOM_CONFIG_DIR` is set | `$FLEETCOM_CONFIG_DIR/sessions` |
| Linux default | `${XDG_CONFIG_HOME:-~/.config}/fleetcom/sessions` |
| macOS default | `~/Library/Application Support/fleetcom/sessions` |

The first save creates the directory. This matches the [configuration path resolution](README.md#config-directory-sessions) used by save, list, and load.

The filename derives from the session name. Fleetcom trims leading and trailing whitespace, replaces control characters and any of `* " / \ < > : | ? .` with `_`, and caps the result at 255 characters. As a result, `my/session` becomes `my_session.json`, while `a.b` becomes `a_b.json`. Replacing `.` prevents the session name from supplying another extension.

## Format

A session is a JSON object that maps each working directory to an ordered list of entries. An entry with neither a group nor a name is a command string. An entry carrying either is an object with `cmd` plus the optional `group` and `name` fields:

```json
{
  "/home/you/work/api": [
    "cargo watch -x test",
    { "cmd": "cargo run", "group": "api", "name": "api server" }
  ],
  "/tmp": [
    "top"
  ]
}
```

- Keys are directory paths: each task's working directory.
- Values are ordered lists. A string member is a bare shell command; the object form adds the group and display name its task receives on load, each written only when set. Order is preserved, and each command runs in its own PTY under that directory.
- Directories serialize alphabetically. Command order remains stable within each directory.

The schema is a flat map with no version field or metadata, so it remains practical to edit by hand. On load, the daemon normalizes group and display names alike: control characters removed, whitespace trimmed, a 64-character cap. `Unassigned` maps to no group but remains a legal display name. Invalid JSON fails the entire load. Within valid JSON, Fleetcom drops any member that matches neither entry form, including a non-string scalar, an object without a string `cmd`, or an object with a non-string `group` or `name`.

Commands with neither a group nor a name are written as strings, so a fleet without either produces a file identical to the pre-group format. Both forms can appear in the same directory array, and files that predate groups and names load unchanged.

## Saving and loading

- Save: `w` in the dashboard, type a name, `Enter`. Writes each task's directory, command, and optional group and name to `<name>.json`.
- Load in-app: `o`, pick from the list, `Enter`.
- Load at launch: `fleetcom <name>`.

Loading always spawns new processes from the stored commands. Existing jobs remain daemon state and never become part of the session file.
