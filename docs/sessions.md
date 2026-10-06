# Sessions

A session is a launch recipe: commands, working directories, and each task's optional group assignment and display name. Loading a session always starts new processes. To keep live processes running across client disconnects, use the [daemon](README.md#storage-paths).

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

- A **literal** entry is a typed command. Use a bare string without labels, or an object with `cmd` and optional `group` and `name` fields. On load, the stored text is run unchanged through your shell: even `"claude"` is a literal command.
- A **managed** entry is an object with `agent`, the registered program word (`claude`, `codex`, `grok`, or `omp`), plus optional `resume`, `group`, and `name` fields. Tasks launched from the Agent page are saved in this form. On load, the agent is launched as managed; with `resume`, the stored session ID is reopened, and without it, a fresh conversation is started. See [Agent session resume](agent-resume.md).

```json
{
  "version": 2,
  "name": "work/api",
  "dirs": {
    "~/work/api": [
      "cargo watch -x test",
      { "cmd": "cargo run", "group": "api", "name": "api server" },
      { "agent": "claude", "resume": "c8c4a5cc-0b32-4ba0-a6b4-6ed08c218e0d", "group": "agents" },
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

The `version` field must be an integer from 1 through the newest format supported by the running `fleetcom`. A missing field means version 1. Invalid or unsupported versions are refused on load, with the file's value and supported version in the error. A version 2 file read by an older `fleetcom` is refused by that same gate.

The schema is identified by shape, independently of the version. With an object-valued `dirs`, the wrapped form above is used. A flat map is also accepted, with directories as top-level keys and entry arrays as values. In that form, an array-valued key named `dirs` remains a directory entry, but a top-level `version` member is always the format version, never a directory. Flat-map files are listed by filename stem because no name is stored. On save, the wrapped form is written, without a stored-name collision check.

Saves are atomic: a private temporary file is written and synced in the session directory, then renamed over the recipe. Full command lines are persisted, including any embedded secrets. See [Security](README.md#security) for directory and file permissions.

The file is plain JSON and practical to edit by hand. Edit `name` to change the stored session identity: a subsequent save under the old name will be refused by the collision check. A readable stored name is still used for collision checks and picker labels when the command body is invalid or the version unsupported.

The entire recipe is validated before any commands are started. The root must be an object, every directory value must be an array, and every entry must be a command string or an object with a string `cmd` or a string `agent`, never both. `agent` must be a supported program word, and `resume`, when present, must be a lowercase `8-4-4-4-12` UUID or `null`. Strings, `null`, or omission are accepted for optional entry `group` and `name` fields and the wrapped session's `name`; other types are rejected. Unknown fields in wrapped metadata and entry objects are ignored. Empty maps, arrays, and strings are valid. With invalid JSON or any malformed field, the whole load is refused; existing tasks and the recipe file are left intact. In schema errors, the quoted directory key, entry number (starting at 1), and offending field are included where applicable.

After validation, group and display names are stripped of control characters and surrounding whitespace and limited to 64 characters. `Unassigned` is treated as no group but is a legal display name. Missing directories, entries past the task limit, and literal commands past the length limit are skipped and counted in the load status. A managed entry whose agent is not found on the connected client's `PATH` is reported on the status line and counted as a failed spawn; the remaining entries are still loaded. Spawn failures are reported separately; tasks already started from a structurally valid recipe are left running.

Use the string form for commands with neither a group nor a name. String and object entries can appear in the same directory array.

### Loading version 1 files

In format 1, instrumented tasks were stored as command strings: a bare program word, or the word followed by its resume selector and a single-quoted UUID. When loading a file with `version` 1 or no version field, convert exactly those two forms to managed entries:

| Version 1 entry | Loads as |
| -- | -- |
| `claude`, `codex`, `grok`, or `omp` alone | managed, fresh conversation |
| `claude --resume '<uuid>'`, `codex resume '<uuid>'`, `grok --resume '<uuid>'`, or `omp --resume '<uuid>'`, with or without the quotes | managed, resuming that ID |
| anything else | literal, unchanged |

Tabs and repeated spaces between tokens are still accepted. Commands with paths (`/usr/local/bin/claude`, `~/bin/claude`) are loaded as literals to preserve your chosen binary instead of substituting one found on `PATH`; managed resume tracking is then unavailable. Commands with extra arguments, prompts, other resume spellings, or shell syntax remain literal, as before. No conversion is applied to version 2 files: a literal `claude` stays literal.

Recovery snapshots are rewritten continually in the new format; named sessions are converted on the next save. This conversion will be removed one release after format 2 ships. After that, all entries in unconverted version 1 files will be loaded as literals.

## Recovery

The current task set is automatically snapshotted under `recovery/` inside the session directory. A separate file is written for each daemon (or `--foreground` core), named for its start time and process ID. After each write, the new file and files whose process IDs are still live are protected; among the remaining files, the nine newest names are retained. A shared recovery directory can therefore contain more than ten snapshots while multiple writers are live.

A snapshot pass is scheduled two seconds after the last command that can change a saved recipe, coalescing a burst of commands. A nonempty recipe is written when its content or destination changed, or when the expected snapshot file is missing. Content changes that no command caused, such as a managed task's newly captured session ID, are also checked every 60 seconds.

- No snapshot is written for an empty fleet. After removing every task, the previous snapshot is retained.
- Snapshots are left in place after quitting, disconnecting, or `fleetcom --kill`.

Snapshots are stored in the session format above, with an `autosaved <timestamp>` UTC label in `name`. Load one to start its commands as with a named session, then save the recovered fleet under a permanent name when prompted.

In the dashboard, press `o` to open the [session picker](commands.md#the-o-session-picker) on the saved list, then `Tab` to view available recovery snapshots.

As with saved recipes, full command lines are persisted in recovery files, including any embedded secrets.

## Saving and loading

- Save: press `w` in the dashboard, type a name, and press `Enter` to write `<name>.json` with the session name and each task's directory, command or managed agent and session ID, and optional group and name.
- Load in-app: `o`, pick from the list, `Enter`.
- Load at launch: `fleetcom <name>`.

On load, new processes are always spawned from the stored commands. Live process state is retained by the daemon and never stored in session files. See [Agent session resume](agent-resume.md) for preserving supported agent conversations across relaunch.
