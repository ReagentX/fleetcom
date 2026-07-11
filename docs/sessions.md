# Sessions

A session is a **recipe**: a set of `{directory: [commands]}` pairs you can save and replay. Loading one re-runs its commands; it does not resurrect live processes — keeping running jobs alive across disconnects is the [daemon](README.md#directory--environment-configuration)'s job, not a session's.

## Storage

Sessions live under the [config directory](README.md#config-directory-sessions), one file per session:

```text
<config>/fleetcom/sessions/<name>.json
```

`<config>` is `$FLEETCOM_CONFIG_DIR` if set, otherwise the platform config directory (`~/.config/fleetcom` on Linux, `~/Library/Application Support/fleetcom` on macOS). It's created on the first save.

The filename is the session name, sanitized: leading and trailing whitespace trimmed, then every control character and any of `* " / \ < > : | ? .` replaced with `_`, capped at 255 characters. So `my/session` is stored as `my_session.json`, and `a.b` as `a_b.json` — `.` is replaced too, so a name can't smuggle in its own extension.

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

- **Keys** are directory paths — each task's working directory.
- **Values** are ordered lists of shell command strings. Order is preserved, and each command runs in its own PTY under that directory.
- **Directories serialize alphabetically** (the on-disk form is a `BTreeMap`), so a session's file is stable no matter what order you added the tasks in — clean diffs, git-friendly.

Hand-editing is fine. The schema is a flat map — no version field, no metadata. An unparseable file is an error at load; anything that isn't a `string → [string]` entry is skipped.

## Saving and loading

- **Save** — `w` in the dashboard, type a name, `Enter`. Writes the current task set (each task's directory and command) to `<name>.json`.
- **Load in-app** — `o`, pick from the list, `Enter`.
- **Load at launch** — `fleetcom <name>`.

Loading spawns every command fresh — it's a *replay*, not a restore. You get new processes running the same commands, not the exact processes you had when you saved. (Live jobs already outlive a disconnect on their own; that's the daemon, not the session.)
