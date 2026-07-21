%{!?conduit_version:%global conduit_version 0.1.0}

Name:           conduit
Version:        %{conduit_version}
Release:        1%{?dist}
Summary:        Focused native Slack client for GNOME
License:        GPL-3.0-or-later
URL:            https://github.com/adrighem/Conduit
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  appstream
BuildRequires:  cargo >= 1.88
BuildRequires:  cmake
BuildRequires:  desktop-file-utils
BuildRequires:  gcc
BuildRequires:  gettext
BuildRequires:  meson
BuildRequires:  pkgconfig(dbus-1)
BuildRequires:  pkgconfig(gdk-pixbuf-2.0)
BuildRequires:  pkgconfig(gtk4)
BuildRequires:  pkgconfig(libadwaita-1)
BuildRequires:  pkgconfig(webkitgtk-6.0)
BuildRequires:  rust >= 1.88

Requires:       ca-certificates
Requires:       xdg-desktop-portal
Requires:       xdg-utils

%description
Conduit is a Rust, GTK4, libadwaita, and WebKitGTK desktop client for
everyday Slack conversations, threads, search, files, and huddles.

%prep
%autosetup

%build
%meson \
    --buildtype=release \
    -Dnative_media=disabled \
    -Dscreen_share=disabled
CARGO_HOME="%{_vpath_builddir}/cargo-home" cargo fetch --locked
CARGO_NET_OFFLINE=true %meson_build

%install
%meson_install

%check
CARGO_NET_OFFLINE=true %meson_test

%files
%doc %{_docdir}/%{name}/README.md
%license %{_datadir}/licenses/%{name}/LICENSE
%{_bindir}/conduit
%{_datadir}/applications/eu.vanadrighem.conduit.desktop
%{_datadir}/conduit/conduit.gresource
%{_datadir}/dbus-1/services/eu.vanadrighem.conduit.service
%{_datadir}/glib-2.0/schemas/eu.vanadrighem.conduit.gschema.xml
%{_datadir}/gnome-shell/search-providers/eu.vanadrighem.conduit.search-provider.ini
%{_datadir}/icons/hicolor/*/apps/eu.vanadrighem.conduit.*
%{_datadir}/metainfo/eu.vanadrighem.conduit.metainfo.xml

%changelog
* Mon Jul 20 2026 Vincent van Adrighem <vincent@vanadrighem.eu> - 0.1.0-1
- Add automated GitHub release packaging.
