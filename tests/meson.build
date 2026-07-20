python = import('python').find_installation('python3')
xvfb_run = find_program('xvfb-run', required: false)
xdotool = find_program('xdotool', required: false)
xclip = find_program('xclip', required: false)
xfwm4 = find_program('xfwm4', required: false)
xprop = find_program('xprop', required: false)

test_service_conf = configuration_data()
test_service_conf.set('bindir', meson.project_build_root() / 'src')
configure_file(
  input: '../data/eu.vanadrighem.conduit.service.in',
  output: 'eu.vanadrighem.conduit.service',
  configuration: test_service_conf,
)

test(
  'Search provider metadata',
  python,
  args: [files('test_search_provider_metadata.py'), meson.project_source_root() / 'data'],
)

if xvfb_run.found() and xfwm4.found()
  test(
    'External Slack URI activation',
    python,
    args: [
      files('run_headless.py'),
      files('test_slack_uri_activation.py'),
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

  test(
    'Localized message timestamps',
    python,
    args: [
      files('run_headless.py'),
      files('test_timestamp_localization.py'),
      meson.project_source_root() / 'src' / 'timestamp_localization.js',
      meson.project_source_root() / 'src' / 'timeline_dom_runtime.js',
    ],
    env: [
      'GDK_DEBUG=no-portals',
      'GDK_BACKEND=x11',
      'GSK_RENDERER=cairo',
      'GTK_A11Y=none',
      'TZ=Europe/Amsterdam',
      'XDG_SESSION_TYPE=x11',
    ],
    timeout: 90,
  )

  test(
    'Timeline scroll anchoring',
    python,
    args: [
      files('run_headless.py'),
      files('test_timeline_scroll.py'),
      meson.project_source_root() / 'src' / 'timeline_dom_runtime.js',
    ],
    env: [
      'GDK_DEBUG=no-portals',
      'GDK_BACKEND=x11',
      'GSK_RENDERER=cairo',
      'GTK_A11Y=none',
      'XDG_SESSION_TYPE=x11',
    ],
    timeout: 90,
  )

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
      'CONDUIT_TEST_DBUS_SERVICE_DIR=' + meson.current_build_dir(),
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

  if xdotool.found() and xprop.found()
    test(
      'Window state persistence',
      python,
      args: [
        files('run_headless.py'),
        files('test_window_state.py'),
      ],
      env: [
        'CONDUIT_TEST_BINARY=' + meson.project_build_root() / 'src' / meson.project_name(),
        'CONDUIT_TEST_RESOURCE=' + meson.project_build_root() / 'src' / 'conduit.gresource',
        'CONDUIT_TEST_SCHEMA=' + meson.project_source_root() / 'data' / 'eu.vanadrighem.conduit.gschema.xml',
        'GSETTINGS_BACKEND=keyfile',
        'GDK_DEBUG=no-portals',
        'GDK_BACKEND=x11',
        'GSK_RENDERER=cairo',
        'GTK_A11Y=none',
        'XDG_SESSION_TYPE=x11',
      ],
      depends: [cargo_build],
      timeout: 90,
    )

    test(
      'Huddle UI smoke',
      python,
      args: [
        files('run_headless.py'),
        files('test_huddle_ui.py'),
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
endif

if xvfb_run.found() and xdotool.found() and xclip.found() and xfwm4.found()
  test(
    'Keyboard shortcuts and emoji completion',
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
