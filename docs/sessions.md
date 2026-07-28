# Sessions

A session is a launch recipe, not a process snapshot. It records commands, working directories, and each task's optional group assignment and display name. Loading always starts new processes. Live process continuity belongs to the [daemon](README.md#storage-paths), which keeps tasks running across client disconnects.

## Storage

`fleetcom` stores one JSON file per session. The session directory resolves in this order:

| Condition | Session directory |
| -- | -- |
| `FLEETCOM_CONFIG_DIR` is set | `$FLEETCOM_CONFIG_DIR/sessions` |
| Linux default | `${XDG_CONFIG_HOME:-~/.config}/fleetcom/sessions` |
| macOS default | `~/Library/Application Support/fleetcom/sessions` |

The first save creates the directory. This matches the [configuration path resolution](README.md#config-directory-sessions) used by save, list, and load.

The filename derives from the session name. `fleetcom` trims leading and trailing whitespace, replaces control characters and any of `* " / \ < > : | ? .` with `_`, and limits the sanitized stem to 250 UTF-8 bytes so the `.json` filename fits within 255 bytes. The cap falls on a character boundary. `my/session` becomes `my_session.json`, and `a.b` becomes `a_b.json`. Replacing `.` prevents the session name from supplying another extension.

## Format

A session file is a JSON object with three fields. `version` is the format version, currently 1. `name` holds the session name as typed, trimmed but not sanitized. `dirs` maps each working directory to an ordered list of entries. An entry with neither a group nor a name is a command string. An entry carrying either is an object with `cmd` plus the optional `group` and `name` fields:

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

- `name` exists because sanitization collapses distinct session names onto one filename: `a/b` and `a.b` both save to `a_b.json`. Saving compares the stored name against the incoming one and refuses a mismatch with an error naming both sessions. The load picker also displays it, so the list shows `a/b`, not `a_b`.
- Keys under `dirs` are directory paths: each task's working directory. Saves write directories under `$HOME` as `~/...`; other paths stay absolute. On load, `~` expands to `$HOME`, and a relative key resolves against the invocation directory of the client loading the session.
- Values are ordered lists. A string member is a bare shell command; the object form adds the optional group and display name assigned on load. Order is preserved, and each command runs in its own PTY under that directory.
- Directories serialize alphabetically. Command order remains stable within each directory.

The `version` field must be an integer from 1 through the newest format supported by the running `fleetcom`. A missing field means version 1. Invalid or unsupported versions fail to load, and the error reports the file's value and the supported version.

The shape, not the version, discriminates the schema. An object-valued `dirs` marks the wrapped form shown above. The loader also accepts a flat map whose top-level keys are directories and whose values are entry arrays. In that form, an array-valued key named `dirs` remains a directory entry, but a top-level `version` member is always the format version, never a directory. Flat-map files list by filename stem because they have no stored name. Saving one writes the wrapped form and permits overwriting it without a stored-name collision check.

Saves are atomic: `fleetcom` writes and syncs a private temporary file in the session directory, then renames it over the recipe. Recipes persist full command lines, which can embed secrets. [Security](README.md#security) documents the directory and file permissions.

The file is plain JSON and practical to edit by hand. Editing the `name` field changes which session the file claims to be: collision checks compare it, so a save under the old name will be refused. On load, the daemon removes control characters, trims surrounding whitespace, and limits group and display names to 64 characters. `Unassigned` maps to no group but remains a legal display name. Invalid JSON fails the entire load. Within valid JSON, `fleetcom` drops any member that matches neither entry form, including a non-string scalar, an object without a string `cmd`, or an object with a non-string `group` or `name`.

Commands with neither a group nor a name use the string form. String and object entries can appear in the same directory array.

A bare agent command does not identify its conversation, so saving it verbatim would start another one on load. When `fleetcom` captures an ID for `claude`, `codex`, or `grok`, it stores the resume form instead. The result remains an ordinary command string that can run directly in a shell:

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

`fleetcom` automatically snapshots the current task set under `recovery/` inside the session directory. Each daemon (or `--foreground` core) writes to a file named for its start time and process ID. After each write, the new file and files whose process IDs are still live are protected; among the remaining files, the nine newest names survive. A shared recovery directory can therefore contain more than ten snapshots while multiple writers are live.

A snapshot pass runs two seconds after the last command that can change a saved recipe, coalescing a burst of commands. A pass writes a nonempty recipe when its content or destination changed, or when the expected snapshot file is missing. Every 60 seconds, `fleetcom` also checks for stored-command changes such as a newly captured agent resume ID.

- An empty fleet does not write a snapshot, so removing every task does not replace the previous snapshot with an empty recipe.
- Quitting, disconnecting, and `fleetcom --kill` leave snapshots in place.

A snapshot uses the session format above, with an `autosaved <timestamp>` UTC label in its `name` field. Loading one spawns its commands like a named session and suggests saving the recovered fleet under a permanent name.

In the dashboard, `o` opens the [session picker](commands.md#the-o-session-picker) on the saved list; while snapshots exist, `Tab` flips it to the recovery list.

Recovery files carry the same caveat as saved recipes: they persist full command lines, which can embed secrets.

## Saving and loading

- Save: `w` in the dashboard, type a name, `Enter`. Writes the session name plus each task's directory, command, and optional group and name to `<name>.json`.
- Load in-app: `o`, pick from the list, `Enter`.
- Load at launch: `fleetcom <name>`.

Loading always spawns new processes from the stored commands. Existing tasks remain daemon state and never enter the session file. [Agent session resume](agent-resume.md) documents when supported agent commands can preserve their conversations across that relaunch.
