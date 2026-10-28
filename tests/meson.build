python_module = import('python')
python = python_module.find_installation('python3')
gi_python = python_module.find_installation(
  '/usr/bin/python3',
  required: false,
  modules: ['gi'],
)
if not gi_python.found()
  gi_python = python_module.find_installation(
    'python3',
    required: false,
    modules: ['gi'],
  )
endif
if not gi_python.found()
  error('Branding validation requires Python 3 with PyGObject')
endif

gdk_pixbuf_gi_probe = run_command(
  gi_python,
  '-c',
  'import gi; gi.require_version("GdkPixbuf", "2.0"); from gi.repository import GdkPixbuf',
  check: false,
)
if gdk_pixbuf_gi_probe.returncode() != 0
  error('Branding validation requires GdkPixbuf 2.0 introspection')
endif

xvfb_run = find_program('xvfb-run', required: false)
xdotool = find_program('xdotool', required: false)
xclip = find_program('xclip', required: false)
xfwm4 = find_program('xfwm4', required: false)
xprop = find_program('xprop', required: false)
headless_tests_option = get_option('headless_tests')

if headless_tests_option.enabled() and \
   not (xvfb_run.found() and xdotool.found() and xclip.found() and \
        xfwm4.found() and xprop.found())
  error('headless_tests=enabled requires xvfb-run, xdotool, xclip, xfwm4, and xprop')
endif

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
  suite: 'metadata',
)

test(
  'Release automation metadata',
  python,
  args: [
    files('test_release_automation.py'),
    meson.project_source_root(),
    meson.project_build_root(),
  ],
  suite: 'metadata',
)

test(
  'Branding assets',
  gi_python,
  args: [
    files('test_branding_assets.py'),
    meson.project_source_root(),
    meson.project_build_root(),
    get_option('prefix'),
    get_option('datadir'),
  ],
  suite: 'metadata',
)

test(
  'Notification preference schema',
  gi_python,
  args: [
    files('test_notification_preferences.py'),
    meson.project_source_root() / 'data' / 'eu.vanadrighem.conduit.gschema.xml',
  ],
  suite: 'metadata',
)

if not headless_tests_option.disabled() and xvfb_run.found()
  # Cargo tests skip GTK widget tests when no display is available; this runs
  # the rendering tests under Xvfb and makes a missing display fail loudly.
  test(
    'Visual rendering',
    xvfb_run,
    # `env` layers the display settings over cargo_env, which must match the
    # other cargo invocations so the test build is reused, not redone.
    args: [
      '-a', 'env',
      'CONDUIT_REQUIRE_DISPLAY=1',
      'GSETTINGS_BACKEND=memory',
      'GDK_BACKEND=x11',
      'GSK_RENDERER=cairo',
      'GTK_A11Y=none',
      cargo_bin.full_path(),
    ] + cargo_test_args + ['--', 'visual_tests'],
    env: cargo_env,
    depends: [cargo_build],
    suite: ['headless', 'visual'],
    timeout: 1800,
    is_parallel: false,
  )

  # Count-based guards against GTK-thread hitches: full timeline rebinds,
  # gdk-pixbuf/glycin header probes, synchronous sidebar renders and
  # uncapped startup prefetch. See AGENTS.md.
  test(
    'Performance guards',
    xvfb_run,
    args: [
      '-a', 'env',
      'CONDUIT_REQUIRE_DISPLAY=1',
      'GSETTINGS_BACKEND=memory',
      'GDK_BACKEND=x11',
      'GSK_RENDERER=cairo',
      'GTK_A11Y=none',
      cargo_bin.full_path(),
    ] + cargo_test_args + ['--', 'perf_guard'],
    env: cargo_env,
    depends: [cargo_build],
    suite: ['headless', 'performance'],
    timeout: 600,
    is_parallel: false,
  )
endif

if not headless_tests_option.disabled() and xvfb_run.found() and xfwm4.found() and \
   (xprop.found() or xdotool.found())
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
    suite: 'headless',
    timeout: 90,
    is_parallel: false,
  )


  test(
    'GNOME search provider',
    gi_python,
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
    suite: 'headless',
    timeout: 90,
    is_parallel: false,
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
      suite: 'headless',
      timeout: 90,
      is_parallel: false,
    )
  endif

  if xdotool.found()
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
      suite: ['headless', 'huddle-ui'],
      timeout: 90,
      is_parallel: false,
    )

    test(
      'Status dialog and current-user header',
      python,
      args: [
        files('run_headless.py'),
        files('test_status_dialog.py'),
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
      suite: ['headless', 'status-ui'],
      timeout: 90,
      is_parallel: false,
    )
  endif
endif

if not headless_tests_option.disabled() and xvfb_run.found() and xdotool.found() and \
   xclip.found() and xfwm4.found()
  test(
    'Keyboard shortcuts and composer completion',
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
    suite: 'headless',
    timeout: 90,
    # Synthetic input focus races concurrent Xvfb and xfwm4 startup.
    is_parallel: false,
  )
endif
