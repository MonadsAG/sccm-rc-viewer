# Script interface — live test checklist (dev till, user present)

Unit tests cover the logic; these checks need a real till and a person.
Use the dev till only. Close every session cleanly at the end.

## Viewer: aspect ratio (separate fix)
1. Maximize the viewer window: the till image keeps its proportions, grey bars at the sides, text not stretched.
2. Press a mouse button on the desktop, drag into a bar, release: nothing stays pressed on the till.

## Headless mode (`sccm-rc-mcp --mode headless`)
1. Register: `claude mcp add sccm-rc -- "<path>/SCCM RC.app/Contents/MacOS/sccm-rc-mcp" --mode headless`.
2. Password saved in the Keychain by the viewer (or `SCCM_PASS`); `connect` to the dev till → connected, no prompt.
3. `screenshot` → till image; `click` on a visible button using the image coordinates → hits it.
4. Open the till login; `type_text` user, then password with `secret: true`; one umlaut in a text field. Check: does the umlaut arrive (Unicode input)? If not → note it (fallback: Swiss layout, plan Task 4 note).
5. Ring up a test article via the on-screen digit buttons with `batch`; `wait` with `stable_ms` before reading the result.
6. While the viewer is connected to the same till: headless `connect` → "another remote-control session is active"; viewer untouched.
7. With a user logged on at the till console (if testable): `connect` → "in use … use force"; `connect(force: true)` → take-over, audit line "take-over of a logged-on user".
8. `disconnect` → then the viewer connects without "existing session".
9. Close the terminal running Claude Code mid-session → till released (viewer connects afterwards).
10. Audit log (`~/Library/Application Support/sccm-rc/audit.jsonl`): actions listed, secret only as "type_text (secret, N chars)".

## Live mode (viewer + `sccm-rc-mcp --mode live`)
1. Viewer start: Control menu "Allow Control by Claude" unchecked; no `control.sock`.
2. Switch on (menu or toolbar): checked, socket `srw-------`, audit "control by Claude allowed".
3. Without a till session: `status` → connected false.
4. Connect the viewer to the dev till; Claude `status` → mode live, target, size; `screenshot` works.
5. Claude `click` → subtitle "Controlled by Claude" for ~5 s.
6. Claude `type_text` long text → typed in bursts.
7. Long `batch`; press a key yourself mid-way → stops, `user_took_over` with `completed_steps`; Shift/Ctrl not stuck.
8. Within 2 s after your input Claude is rejected; after 2 s it works. Moving the mouse alone does not interrupt.
9. View-only on: Claude `key` → "view-only mode" error; `screenshot`/`wait` still work.
10. Toggle off during a `wait` → `control_disabled`, socket gone; Claude calls → "enable 'Allow control by Claude'".
11. Cmd+Q with toggle on → socket removed, till released. Restart → toggle off again.
