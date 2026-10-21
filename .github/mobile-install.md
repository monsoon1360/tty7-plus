## Phone app

These downloads are the legacy Tauri/iroh phone client. The desktop **Settings → Mobile** page has been removed; for development, run the [gateway CLI manually](../mobile/README.md#run-it) and pair using its `tty7pair:` code. **Settings → Pair Devices** uses a different protocol and cannot pair this client. For the native Android app, follow [Remote Workspaces](../docs/remote/workspaces.mdx).

- **iPhone**: join the beta at https://testflight.apple.com/join/gFcPbPVR. The link asks you to install TestFlight from the App Store first, if you don't have it.
- **Android** (arm64): on the phone, download `tty7-<version>-android-arm64.apk` from the newest [`mobile-v*` release](https://github.com/l0ng-ai/tty7/releases?q=mobile-v&expanded=true) and open it. Android asks once to allow installing apps from your browser or file manager. A later APK installs over it and keeps your paired machines.
