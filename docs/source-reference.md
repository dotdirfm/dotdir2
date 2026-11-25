# Source reference

The native port uses the working tree at `/Users/mike/github/dotdirfm/dotdir`,
including uncommitted changes as of 24 November 2026. The original is left intact.

`crates/dotdir-core` is copied from that working tree, including the atomic
no-replace rename implementation and its regression tests. Changes to this copy
belong to the native port; there is no runtime dependency on the sibling checkout.

Visual reference: `packages/ui/lib/styles/base.module.css`,
`packages/file-list/src/FileList/FileList.module.css`,
`packages/ui/lib/components/Tabs/Tabs.module.css`, and the command/keybinding
contributions under `packages/ui/lib/features/commands`.

The web UI uses 13 px system text, 26 px file rows, 28 px panel tabs, a 4 px pane
gap, 26 px file information footers, and Catppuccin Mocha/Latte base colors.
The native interface follows these measurements and keeps cursor and marked
selection separate.

The running reference app uses an Ayu Mirage theme override: background `#1f2430`,
foreground `#fefaecc4` (composited over that background), accent `#ffcc66`, and
selection `#63759926`. The native default follows that active theme; Mocha and
Latte are alternatives in Settings.
