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
cargo_env  = [
  'CARGO_HOME=' + meson.project_build_root()  / 'cargo-home',
  'CONDUIT_LOCALEDIR=' + get_option('prefix') / get_option('localedir'),
  'CONDUIT_PKGDATADIR=' + pkgdatadir,
  'CONDUIT_SLACK_CLIENT_ID=' + get_option('slack_client_id'),
]

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
    'release_arg="$7"; if [[ -n "$release_arg" ]]; then env "$1" "$2" "$3" "$4" "$5" build --manifest-path "$6" --target-dir "$8" "$release_arg"; else env "$1" "$2" "$3" "$4" "$5" build --manifest-path "$6" --target-dir "$8"; fi && cp "$8/$9/${10}" "${11}"',
    'cargo-build',
    cargo_env[0],
    cargo_env[1],
    cargo_env[2],
    cargo_env[3],
    cargo_bin.full_path(),
    meson.project_source_root() / 'Cargo.toml',
    cargo_release_arg,
    meson.project_build_root() / 'src',
    rust_target,
    meson.project_name(),
    '@OUTPUT@',
  ]
)

test('Cargo tests',
  cargo_bin,
  args: [
    'test',
    '--manifest-path', meson.project_source_root() / 'Cargo.toml',
    '--target-dir', meson.project_build_root() / 'src',
  ],
  env: cargo_env,
  timeout: 300,
  depends: cargo_build,
)
