//! Replay release-binary output; the model responses were scripted locally.

use crate::{emulator::Emulator, protocol::ClipboardKind};

const REPLY: &str = "PHASE0 COMPLETE. Deterministic local response.";
const PROMPT: &str = "Phase zero fixture: reply with the completion marker.";
const MANIFEST: &str = include_str!("../../tests/corpus/codex_0158_terminal/manifest.json");
const STREAMS: [&[u8]; 4] = [
    include_bytes!("../../tests/corpus/codex_0158_terminal/0.157.1-fullscreen-transitions.bin"),
    include_bytes!("../../tests/corpus/codex_0158_terminal/0.157.1-fullscreen-fleetcom-load.bin"),
    include_bytes!("../../tests/corpus/codex_0158_terminal/0.158.0-fullscreen-transitions.bin"),
    include_bytes!("../../tests/corpus/codex_0158_terminal/0.158.0-fullscreen-fleetcom-load.bin"),
];

#[test]
fn codex_release_terminal_transitions() {
    let manifest = jzon::parse(MANIFEST).unwrap();
    assert_eq!(manifest["fixtures"].len(), STREAMS.len());
    for (fixture, bytes) in manifest["fixtures"].members().zip(STREAMS) {
        let name = fixture["name"].as_str().unwrap();
        let outer = fixture["surface"] == "outer fleetcom client";
        assert_eq!(fixture["bytes"].as_usize().unwrap(), bytes.len(), "{name}");
        // Small chunks also split UTF-8 and escape sequences across parser calls.
        for chunk_size in [usize::MAX, 7] {
            let mut emu = Emulator::new(40, 120, 10_000);
            assert!(!emu.alternate_screen());
            let mut at = 0;
            let mut resizes = fixture["resizes"].members().peekable();
            for mark in fixture["marks"].members() {
                let checkpoint = mark["name"].as_str().unwrap();
                let end = mark["offset"].as_usize().unwrap();
                // A resize at a checkpoint's end belongs to the following state.
                while let Some(resize) = resizes.peek() {
                    let offset = resize["offset"].as_usize().unwrap();
                    if offset >= end {
                        break;
                    }
                    for chunk in bytes[at..offset].chunks(chunk_size) {
                        emu.process(chunk);
                    }
                    emu.resize(
                        resize["resize"][0].as_u16().unwrap(),
                        resize["resize"][1].as_u16().unwrap(),
                    );
                    at = offset;
                    resizes.next();
                }
                for chunk in bytes[at..end].chunks(chunk_size) {
                    emu.process(chunk);
                }
                at = end;
                let context = format!("{name}/{checkpoint}, chunk size {chunk_size}");
                assert_eq!(
                    emu.size(),
                    (
                        mark["rows"].as_u16().unwrap(),
                        mark["cols"].as_u16().unwrap()
                    ),
                    "{context}"
                );
                assert_eq!(emu.alternate_screen(), checkpoint != "exit", "{context}");
                let text = emu.live_rows().join("\n");
                match checkpoint {
                    "attached" | "completed" | "copied" | "narrow" | "restored" => {
                        assert!(text.contains(PROMPT), "{context}: missing prompt: {text}");
                        assert!(text.contains(REPLY), "{context}: missing reply: {text}");
                        if outer {
                            assert!(text.contains("[attached] codex resume '"), "{context}");
                            let replies = if checkpoint == "attached" { 1 } else { 2 };
                            assert_eq!(text.matches(REPLY).count(), replies, "{context}");
                        }
                    }
                    "new-thread" | "exit" => {
                        assert!(!text.contains(PROMPT), "{context}: stale prompt");
                        assert!(!text.contains(REPLY), "{context}: stale reply");
                    }
                    "startup" | "copy-menu" => {}
                    other => panic!("{context}: unknown checkpoint {other}"),
                }
                if checkpoint == "copied" {
                    assert!(
                        text.contains("Copied Whole response to clipboard"),
                        "{context}"
                    );
                    if name.starts_with("0.158.0") {
                        assert!(text.contains("Copying Whole response"), "{context}");
                    }
                }
                let clipboard = emu.drain_clipboard();
                assert_eq!(clipboard.oversized_len, None, "{context}");
                let expected = if checkpoint == "copied" {
                    vec![(ClipboardKind::Clipboard, REPLY.to_owned())]
                } else {
                    vec![]
                };
                assert_eq!(clipboard.stores, expected, "{context}");
                assert!(
                    emu.drain_clipboard().stores.is_empty(),
                    "{context}: drained twice"
                );
            }
            assert_eq!(at, bytes.len(), "{name}: untested trailing output");
        }
    }
}
