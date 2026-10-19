# Sessions

Save a session to preserve the task set's working directories, commands or managed agents with their session IDs, group assignments, display names, "in use" tags, flagship mark, and whether each task had finished. You cannot preserve client state (the attached or peeked task, selection, grouping and sort modes, terminal size) or process state (output, timings, exit status) in a session. Each time you load a session, you start new processes for the tasks that were live; a task that had finished is restored as a stopped row, with no process until you press `r`. To keep live processes running across client disconnects, use the [daemon](README.md#storage-paths).

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

A session file is a JSON object with three fields: `version` for the format version, currently 2; `name` for the typed session name, trimmed but not sanitized; and `dirs` for an ordered list of entries per working directory. Use one of two entry forms, according to the task's launch mode:

- A **literal** entry is a typed command. Use a bare string without labels or marks, or an object with `cmd` and optional `group`, `name`, `tagged`, `flagship`, and `stopped` fields. Load a literal entry to run the stored text unchanged through your shell: even `"claude"` is a literal command.
- A **managed** entry is an object with `agent`, the registered program word (`claude`, `codex`, `grok`, or `omp`), plus optional `resume`, `group`, `name`, `tagged`, `flagship`, and `stopped` fields. Save tasks launched from the Agent page in this form. Load a managed entry with `resume` to reopen the stored session ID, or without it to start a fresh conversation. See [Agent session resume](agent-resume.md).

Use the optional booleans `tagged`, `flagship`, and `stopped` on either object form. Set `tagged` to `true` to tag a task ["in use"](commands.md#grouping-and-tagging), `flagship` to `true` to mark the [flagship](commands.md#the-flagship), or `stopped` to `true` to restore the task as a row with no process. To leave any of them unset, use `false`, `null`, or omit the field. On save, include each field only when true; `stopped` is true for every task that had finished or had not yet been started. Mark at most one entry per file as the flagship; with a second, you cannot load the file.

```json
{
  "version": 2,
  "name": "work/api",
  "dirs": {
    "~/work/api": [
      "cargo watch -x test",
      { "cmd": "cargo run", "group": "api", "name": "api server", "tagged": true },
      { "cmd": "cargo build --release", "stopped": true },
      { "agent": "claude", "resume": "c8c4a5cc-0b32-4ba0-a6b4-6ed08c218e0d", "group": "agents", "flagship": true },
      { "agent": "omp" }
    ],
    "/tmp": [
      "top"
    ]
  }
}
```

- The stored `name` distinguishes session names that sanitize to the same filename: `a/b` and `a.b` both become `a_b.json`. Before saving, `fleetcom` compares the stored and incoming names and refuses a mismatch, reporting both names in the error. The load picker displays the stored name as `a/b`, not `a_b`.
- Keys under `dirs` are directory paths: each task's working directory. On save, directories under `$HOME` are written as `~/...`; other paths are kept absolute. On load, `~` is expanded to `$HOME`, and relative keys are resolved against the invocation directory of the client loading the session.
- Values are ordered lists, loaded in order. Each task is started in its own PTY under the specified directory.
- Directories are serialized alphabetically, with entry order preserved within each directory.

The `version` field must be an integer from 1 through the newest format supported by the running `fleetcom`. A missing field means version 1. A version 1 file loads with every entry literal, including `claude` and `claude --resume '<uuid>'`, and they are written back as literals on save; to track such a task's conversation, launch it from the Agent page and save again. Invalid or unsupported versions are refused on load, with the file's value and supported version in the error. A version 2 file read by an older `fleetcom` is refused by that same gate.

The schema is identified by shape, independently of the version. With an object-valued `dirs`, the wrapped form above is used. A flat map is also accepted, with directories as top-level keys and entry arrays as values. In that form, an array-valued key named `dirs` remains a directory entry, but a top-level `version` member is always the format version, never a directory. Flat-map files are listed by filename stem because no name is stored. On save, the wrapped form is written, without a stored-name collision check.

Saves are atomic: a private temporary file is written and synced in the session directory, then renamed over the recipe. Full command lines are persisted, including any embedded secrets. See [Security](README.md#security) for directory and file permissions.

The file is plain JSON and practical to edit by hand. Edit `name` to change the stored session identity: a subsequent save under the old name will be refused by the collision check. A readable stored name is still used for collision checks and picker labels when the command body is invalid or the version unsupported.

On load, expect validation of the entire recipe before any commands are started. Use this schema:

- Use an object for the root and an array for each directory value.
- For each entry, use a command string or an object with a string `cmd` or a string `agent`, never both.
- For `agent`, use a supported program word. For `resume`, use a lowercase `8-4-4-4-12` UUID, `null`, or omit the field.
- For the optional entry `group` and `name` fields and the wrapped session's `name`, use strings, `null`, or omit the fields.
- For `tagged`, `flagship`, and `stopped`, use booleans, `null`, or omit the fields. Set `flagship` to `true` on at most one entry; otherwise, both entry locations are reported in the error.

You can include unknown fields in wrapped metadata and entry objects; these are ignored. You can also use empty maps, arrays, and strings. With invalid JSON or any malformed field, you cannot load any part of the recipe; your existing tasks and recipe file remain intact. For schema errors, expect the quoted directory key, entry number (starting at 1), and offending field where applicable.

After validation, group and display names are stripped of control characters and surrounding whitespace and limited to 64 characters. `Unassigned` is treated as no group but is a legal display name. Missing directories, entries past the task limit, and literal commands past the length limit are skipped and counted in the load status. For a live managed entry without an agent binary on the connected client's `PATH`, expect a diagnostic on the status line and a failed-spawn count. The remaining entries are still loaded. Spawn failures are reported separately; tasks already started from a structurally valid recipe are left running.

A stopped entry is restored as a row with no process: lifecycle `stopped`, a blank glyph, a blank age cell, and an empty screen under attach and peek. The same directory, task-limit, and length rules apply. Load a stopped managed entry by agent word alone; binary lookup on the connected client's `PATH` is deferred until you press `r`, so you can load the file without the agent installed. Press `r` to start the row: run the stored text for a literal entry, or resume the stored session ID for a managed entry. See [Rerunning tasks](commands.md#rerunning-tasks).

Read each outcome on the status line, with the stopped count before the skipped and failed counts: `loaded 'api': 1 task(s), 1 stopped, 1 skipped (missing dir, task limit, or command too long)`. With nothing spawned and at least one other count, expect no `0 task(s)` clause: `loaded 'api': 1 stopped`. After loading a recovery snapshot, expect `loaded recovery snapshot; save to name it, 1 stopped`.

Use the string form for a live command with no group, no name, and no mark. String and object entries can appear in the same directory array.

## Recovery

The current task set is automatically snapshotted under `recovery/` inside the session directory. A separate file is written for each daemon (or `--foreground` core), named for its start time and process ID. After each write, the new file and files whose process IDs are still live are protected; among the remaining files, the nine newest names are retained. A shared recovery directory can therefore contain more than ten snapshots while multiple writers are live.

Expect a snapshot pass two seconds after the last command that can change a saved recipe, including marking with `m` or `]`. After a burst of commands, count the delay from the last command. You also get a pass after a task exits, without a command, to persist the entry's `stopped` flag and clear any flagship mark on that task. For a nonempty recipe, expect a write when its content or destination changed, or when the expected snapshot file is missing. For changes made without a command, such as a newly captured session ID for a managed task, expect checks every 60 seconds.

- No snapshot is written for an empty fleet. After removing every task, the previous snapshot is retained.
- Snapshots are left in place after quitting, disconnecting, or `fleetcom --kill`.

Snapshots are stored in the session format above, with an `autosaved <timestamp>` UTC label in `name`. Load one to start its commands as with a named session, then save the recovered fleet under a permanent name when prompted.

In the dashboard, press `o` to open the [session picker](commands.md#the-o-session-picker) on the saved list, then `Tab` to view available recovery snapshots.

As with saved recipes, full command lines are persisted in recovery files, including any embedded secrets.

## Saving and loading

- Save: press `w` in the dashboard, type a name, and press `Enter` to write `<name>.json` with the session name and each task's directory, command or managed agent and session ID, optional group and name, "in use" tag, flagship mark, and stopped flag.
- Load in-app: `o`, pick from the list, `Enter`.
- Load at launch: `fleetcom <name>`.

Load a session to start new processes from the live entries and to restore the stopped entries as rows with no process; press `r` on a stopped row to start it. To retain live process state, use the daemon; you cannot store that state in session files. When you load, you keep your existing tasks and marks and recover the tags on every loaded task. You recover a stored flagship only when no task is currently marked, and never on a stopped entry. See [Agent session resume](agent-resume.md) for preserving supported agent conversations across relaunch.
