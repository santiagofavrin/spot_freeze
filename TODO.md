# TODO

All 9 items implemented and committed (2a94ff8..3b9a7d4), reviewed, test suites green
on macOS + Linux (Docker), windows-target cargo check clean. Remaining: on-platform
runtime verification by the user, then this file can be deleted.

## Verify on Windows
- Settings window rebind fields register modifier keys (item 3)
- Zoom hold row rebinding + auto-start checkbox in the settings window
- auto_start on -> re-login -> app starts; off -> entry gone

## Verify on macOS
- Edit Settings opens the file in TextEdit (item 8)
- auto_start LaunchAgent: on -> re-login -> app starts; off -> plist gone
- Fade animation looks right; no black flash before fade-in

## Verify on Linux (Hyprland)
- Spotlight keeps up with fast cursor movement (item 7)
- Fade animation under shm pacing (may degrade to fewer steps by design)
- Unfreeze fade blends to the freeze-time capture (changed content pops back — by design)
