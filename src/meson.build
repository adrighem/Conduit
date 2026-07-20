pkgdatadir = get_option('prefix') / get_option('datadir') / meson.project_name()
gnome = import('gnome')

gnome.compile_resources('conduit',
  'conduit.gresource.xml',
  gresource_bundle: true,
  install: true,
  install_dir: pkgdatadir,
)

cargo_bin  = find_program('cargo')
cargo_release_arg = ''
cargo_features = []
cargo_env = environment()
cargo_env.set('CARGO_HOME', meson.project_build_root() / 'cargo-home')
cargo_env.set('CONDUIT_LOCALEDIR', get_option('prefix') / get_option('localedir'))
cargo_env.set('CONDUIT_PKGDATADIR', pkgdatadir)
cargo_env.set('CONDUIT_SLACK_CLIENT_ID', get_option('slack_client_id'))

pkg_config_paths = get_option('pkg_config_path')
if pkg_config_paths.length() > 0
  cargo_env.prepend('PKG_CONFIG_PATH', ':'.join(pkg_config_paths), separator: ':')
endif

native_media_option = get_option('native_media')
native_media_dependencies = [
  dependency('gstreamer-1.0', version: '>= 1.24', required: native_media_option),
  dependency('gstreamer-sdp-1.0', version: '>= 1.24', required: native_media_option),
  dependency('gstreamer-webrtc-1.0', version: '>= 1.24', required: native_media_option),
]
native_media_enabled = native_media_dependencies[0].found() and \
                       native_media_dependencies[1].found() and \
                       native_media_dependencies[2].found()

if native_media_enabled
  cargo_features += ['native-media']
endif

screen_share_option = get_option('screen_share')
if screen_share_option.enabled() and not native_media_enabled
  error('screen_share requires native_media and its GStreamer dependencies')
endif

screen_share_enabled = native_media_enabled and not screen_share_option.disabled()
if screen_share_enabled
  cargo_features += ['screen-share']
endif

cargo_features_arg = ','.join(cargo_features)

if get_option('buildtype') == 'release'
  cargo_release_arg = '--release'
  rust_target = 'release'
else
  rust_target = 'debug'
endif

cargo_build = custom_target(
  'cargo-build',
  build_by_default: true,
  build_always_stale: true,
  output: meson.project_name(),
  console: true,
  install: true,
  install_dir: get_option('bindir'),
  command: [
    'bash', '-c',
    'args=(build --locked --manifest-path "$2" --target-dir "$5"); [[ -n "$3" ]] && args+=("$3"); [[ -n "$4" ]] && args+=(--features "$4"); "$1" "${args[@]}" && cp "$5/$6/$7" "$8"',
    'cargo-build',
    cargo_bin.full_path(),
    meson.project_source_root() / 'Cargo.toml',
    cargo_release_arg,
    cargo_features_arg,
    meson.project_build_root() / 'src',
    rust_target,
    meson.project_name(),
    '@OUTPUT@',
  ],
  env: cargo_env,
)

cargo_test_args = [
  'test',
  '--locked',
  '--manifest-path', meson.project_source_root() / 'Cargo.toml',
  '--target-dir', meson.project_build_root() / 'src',
]
if cargo_features_arg != ''
  cargo_test_args += ['--features', cargo_features_arg]
endif

test('Cargo tests',
  cargo_bin,
  args: cargo_test_args,
  env: cargo_env,
  timeout: 300,
  depends: cargo_build,
)
