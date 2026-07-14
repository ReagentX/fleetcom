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
- Values are ordered lists. A string member is a bare shell command; the object form adds the optional group and display name assigned on load. Order is preserved, and each command runs in its own PTY under that directory.
- Directories serialize alphabetically. Command order remains stable within each directory.

The schema is a flat map with no version field or metadata, so it remains practical to edit by hand. On load, the daemon removes control characters, trims surrounding whitespace, and limits group and display names to 64 characters. `Unassigned` maps to no group but remains a legal display name. Invalid JSON fails the entire load. Within valid JSON, Fleetcom drops any member that matches neither entry form, including a non-string scalar, an object without a string `cmd`, or an object with a non-string `group` or `name`.

Commands with neither a group nor a name use the string form. String and object entries can appear in the same directory array.

Commands that launch a supported agent CLI (`claude`, `codex`) are saved in their resuming form when Fleetcom can identify the conversation. The entry remains an ordinary command string that can run directly in a shell:

```json
{
  "/home/you/work/api": [
    "claude --resume 'c8c4a5cc-0b32-4ba0-a6b4-6ed08c218e0d'",
    { "cmd": "codex resume '019f5453-de22-7240-b2e5-0d32692aa6d9'", "name": "reviewer" }
  ]
}
```

## Saving and loading

- Save: `w` in the dashboard, type a name, `Enter`. Writes each task's directory, command, and optional group and name to `<name>.json`.
- Load in-app: `o`, pick from the list, `Enter`.
- Load at launch: `fleetcom <name>`.

Loading always spawns new processes from the stored commands. Existing jobs remain daemon state and never become part of the session file. For supported agent commands, [Agent session resume](agent-resume.md) documents the capture and rewrite mechanics.
