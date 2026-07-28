# Installed Branding Consistency

## Summary

Ensure a Meson installation exposes Conduit's current icons and application
branding consistently in the GNOME application grid and the About dialog.

## Requirements

1. Install the current Conduit application icon under the application ID at the
   standard hicolor icon locations.
2. Install the current symbolic icon under the application ID for shell and
   desktop integration that requests a symbolic variant.
3. Keep the desktop entry, application ID, GTK application icon name, and About
   dialog logo aligned with the installed icon name.
4. Refresh relevant desktop and icon caches after a real installation while
   preserving `DESTDIR` staging behavior.
5. Remove or stop installing obsolete Conduit branding files that can continue
   to win icon lookup after an upgrade.
6. Add automated metadata and install-manifest coverage for the canonical
   branding contract.

## Acceptance Criteria

- A clean staged Meson install contains the canonical application and symbolic
  icons at their expected hicolor paths.
- The installed desktop file and About dialog both resolve the canonical
  application icon name.
- Packaging metadata tests fail if branding names or install destinations drift.
- `cargo check`, Rust tests, Meson compile, and Meson tests pass in a sanitized
  environment.
- After `sudo meson install -C _build`, GNOME shows the current Conduit icon in
  the application grid and About dialog.

## Out of Scope

- Redesigning the supplied Conduit artwork.
- Changing the application ID.
- Supporting non-GNOME desktop cache implementations beyond standard
  freedesktop icon and desktop metadata.
