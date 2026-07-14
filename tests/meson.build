python = import('python').find_installation('python3')
xvfb_run = find_program('xvfb-run', required: false)
xdotool = find_program('xdotool', required: false)
xfwm4 = find_program('xfwm4', required: false)

test(
  'Search provider metadata',
  python,
  args: [files('test_search_provider_metadata.py'), meson.project_source_root() / 'data'],
)

if xvfb_run.found() and xfwm4.found()
  test(
    'GNOME search provider',
    python,
    args: [
      files('run_headless.py'),
      files('test_search_provider.py'),
    ],
    env: [
      'CONDUIT_TEST_BINARY=' + meson.project_build_root() / 'src' / meson.project_name(),
      'CONDUIT_TEST_RESOURCE=' + meson.project_build_root() / 'src' / 'conduit.gresource',
      'CONDUIT_TEST_SCHEMA=' + meson.project_source_root() / 'data' / 'eu.vanadrighem.conduit.gschema.xml',
      'GSETTINGS_BACKEND=memory',
      'GDK_DEBUG=no-portals',
      'GDK_BACKEND=x11',
      'GSK_RENDERER=cairo',
      'GTK_A11Y=none',
      'XDG_SESSION_TYPE=x11',
    ],
    depends: [cargo_build],
    timeout: 90,
  )
endif

if xvfb_run.found() and xdotool.found() and xfwm4.found()
  test(
    'Ctrl-K keyboard shortcut',
    python,
    args: [
      files('run_headless.py'),
      files('test_keyboard_shortcuts.py'),
    ],
    env: [
      'CONDUIT_TEST_BINARY=' + meson.project_build_root() / 'src' / meson.project_name(),
      'CONDUIT_TEST_RESOURCE=' + meson.project_build_root() / 'src' / 'conduit.gresource',
      'CONDUIT_TEST_SCHEMA=' + meson.project_source_root() / 'data' / 'eu.vanadrighem.conduit.gschema.xml',
      'GSETTINGS_BACKEND=memory',
      'GDK_DEBUG=no-portals',
      'GDK_BACKEND=x11',
      'GSK_RENDERER=cairo',
      'GTK_A11Y=none',
      'XDG_SESSION_TYPE=x11',
    ],
    depends: [cargo_build],
    timeout: 90,
  )
endif
