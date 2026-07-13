application_id = 'eu.vanadrighem.conduit'

app_icon_sizes = [
  '16x16',
  '24x24',
  '32x32',
  '48x48',
  '64x64',
  '128x128',
  '256x256',
  '512x512',
]

foreach icon_size : app_icon_sizes
  icon_dir = 'hicolor' / icon_size / 'apps'
  install_data(
    icon_dir / ('@0@.png').format(application_id),
    install_dir: get_option('datadir') / 'icons' / icon_dir
  )
endforeach

scalable_icon_dir = 'hicolor' / 'scalable' / 'apps'
install_data(
  scalable_icon_dir / ('@0@.svg').format(application_id),
  install_dir: get_option('datadir') / 'icons' / scalable_icon_dir
)
