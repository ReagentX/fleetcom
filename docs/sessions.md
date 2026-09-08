# Sessions

A session is a launch recipe: commands, working directories, and each task's optional group assignment and display name. On load, new processes are always started. To keep live processes running across client disconnects, use the [daemon](README.md#storage-paths).

## Storage

Each session is stored in one JSON file. The session directory is resolved in this order:

| Condition | Session directory |
| -- | -- |
| `FLEETCOM_CONFIG_DIR` is set | `$FLEETCOM_CONFIG_DIR/sessions` |
| Linux default | `${XDG_CONFIG_HOME:-~/.config}/fleetcom/sessions` |
| macOS default | `~/Library/Application Support/fleetcom/sessions` |

The directory is created on the first save. The same [configuration path resolution](README.md#config-directory-sessions) is used for save, list, and load.

The filename is derived from the session name: leading and trailing whitespace is trimmed, control characters and any of `* " / \ < > : | ? .` are replaced with `_`, and the sanitized stem is limited to 250 UTF-8 bytes at a character boundary. With `.json`, the filename is at most 255 bytes. `my/session` becomes `my_session.json`, and `a.b` becomes `a_b.json`. With `.` replaced, no other extension can be specified in the session name.

## Format

A session file is a JSON object with three fields. `version` is the format version, currently 1. In `name`, the session name is stored as typed, trimmed but not sanitized. Under `dirs`, each working directory is mapped to an ordered list of entries. An entry with neither a group nor a name is a command string. An entry carrying either is an object with `cmd` plus the optional `group` and `name` fields:

```json
{
  "version": 1,
  "name": "work/api",
  "dirs": {
    "~/work/api": [
      "cargo watch -x test",
      { "cmd": "cargo run", "group": "api", "name": "api server" }
    ],
    "/tmp": [
      "top"
    ]
  }
}
```

- Store `name` to distinguish session names sanitized to the same filename: `a/b` and `a.b` are both saved to `a_b.json`. On save, the stored and incoming names are compared; a mismatch is refused with both names in the error. In the load picker, the stored name is displayed as `a/b`, not `a_b`.
- Keys under `dirs` are directory paths: each task's working directory. On save, directories under `$HOME` are written as `~/...`; other paths are kept absolute. On load, `~` is expanded to `$HOME`, and relative keys are resolved against the invocation directory of the client loading the session.
- Values are ordered lists. A string member is a bare shell command; in the object form, an optional group and display name can also be specified for assignment on load. Order is preserved, and each command is run in its own PTY under that directory.
- Directories are serialized alphabetically. Command order remains stable within each directory.

The `version` field must be an integer from 1 through the newest format supported by the running `fleetcom`. A missing field means version 1. Invalid or unsupported versions are refused on load, with the file's value and supported version in the error.

The schema is identified by shape, independently of the version. With an object-valued `dirs`, the wrapped form above is used. A flat map is also accepted, with directories as top-level keys and entry arrays as values. In that form, an array-valued key named `dirs` remains a directory entry, but a top-level `version` member is always the format version, never a directory. Flat-map files are listed by filename stem because no name is stored. On save, the wrapped form is written, without a stored-name collision check.

Saves are atomic: a private temporary file is written and synced in the session directory, then renamed over the recipe. Full command lines are persisted, including any embedded secrets. See [Security](README.md#security) for directory and file permissions.

The file is plain JSON and practical to edit by hand. Edit `name` to change the stored session identity: a subsequent save under the old name will be refused by the collision check. A readable stored name is still used for collision checks and picker labels when the command body is invalid or the version unsupported.

The entire recipe is validated before any commands are started. The root must be an object, every directory value must be an array, and every entry must be a command string or an object with a string `cmd`. Strings, `null`, or omission are accepted for optional entry `group` and `name` fields and the wrapped session's `name`; other types are rejected. Unknown fields in wrapped metadata and entry objects are ignored. Empty maps, arrays, and strings are valid. With invalid JSON or any malformed field, the whole load is refused; existing tasks and the recipe file are left intact. In schema errors, the quoted directory key, entry number (starting at 1), and offending field are included where applicable.

After validation, group and display names are stripped of control characters and surrounding whitespace and limited to 64 characters. `Unassigned` is treated as no group but is a legal display name. Missing directories and entries exceeding task or command limits are still skipped and counted in the load status. Spawn failures are reported separately; commands already started by a structurally valid recipe keep running.

Use the string form for commands with neither a group nor a name. String and object entries can appear in the same directory array.

No conversation ID is specified in a bare agent command; if saved verbatim, a new conversation would be started on load. With a captured ID for `claude`, `codex`, `grok`, or `omp`, the resume form is stored instead. Without a known ID, the authored command is preserved in named saves and recovery snapshots. You can run the resulting command directly in a shell:

```json
{
  "version": 1,
  "name": "agents",
  "dirs": {
    "~/work/api": [
      "claude --resume 'c8c4a5cc-0b32-4ba0-a6b4-6ed08c218e0d'",
      { "cmd": "codex resume '019f5453-de22-7240-b2e5-0d32692aa6d9'", "name": "reviewer" }
    ]
  }
}
```

## Recovery

The current task set is automatically snapshotted under `recovery/` inside the session directory. A separate file is written for each daemon (or `--foreground` core), named for its start time and process ID. After each write, the new file and files whose process IDs are still live are protected; among the remaining files, the nine newest names are retained. A shared recovery directory can therefore contain more than ten snapshots while multiple writers are live.

A snapshot pass is scheduled two seconds after the last command that can change a saved recipe, coalescing a burst of commands. A nonempty recipe is written when its content or destination changed, or when the expected snapshot file is missing. Stored-command changes, such as a newly captured agent resume ID, are also checked every 60 seconds.

- No snapshot is written for an empty fleet. After removing every task, the previous snapshot is retained.
- Snapshots are left in place after quitting, disconnecting, or `fleetcom --kill`.

Snapshots are stored in the session format above, with an `autosaved <timestamp>` UTC label in `name`. Load one to start its commands as with a named session, then save the recovered fleet under a permanent name when prompted.

In the dashboard, press `o` to open the [session picker](commands.md#the-o-session-picker) on the saved list, then `Tab` to view available recovery snapshots.

As with saved recipes, full command lines are persisted in recovery files, including any embedded secrets.

## Saving and loading

- Save: `w` in the dashboard, type a name, `Enter`. Store the session name plus each task's directory, command, and optional group and name to `<name>.json`.
- Load in-app: `o`, pick from the list, `Enter`.
- Load at launch: `fleetcom <name>`.

On load, new processes are always spawned from the stored commands. Live process state is retained by the daemon and never stored in session files. See [Agent session resume](agent-resume.md) for preserving supported agent conversations across relaunch.
