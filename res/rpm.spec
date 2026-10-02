# The package is named after the OEM identity, not the upstream one. NERV Desk (task-69).
Name:       nervdesk
Version:    1.5.0
Release:    0
Summary:    RPM package
License:    GPL-3.0
URL:        https://rustdesk.com
# Obsoletes/Provides are what makes this package replace an installed upstream rustdesk on
# upgrade instead of failing on a file conflict -- both packages own /usr/bin/rustdesk and
# /usr/share/rustdesk. NERV Desk (task-69).
Vendor:     NERVDesk <info@nervdesk.local>
Requires:   gtk3 libxcb libXfixes alsa-lib libva2 gstreamer1-plugins-base
Recommends: libayatana-appindicator-gtk3 libxdo
Obsoletes:  rustdesk
Provides:   rustdesk

# https://docs.fedoraproject.org/en-US/packaging-guidelines/Scriptlets/

%description
The best open-source remote desktop client software, written in Rust.

%prep
# we have no source, so nothing here

%build
# we have no source, so nothing here

%global __python %{__python3}

%install
mkdir -p %{buildroot}/usr/bin/
mkdir -p %{buildroot}/usr/share/rustdesk/
mkdir -p %{buildroot}/usr/share/rustdesk/files/
mkdir -p %{buildroot}/usr/share/icons/hicolor/256x256/apps/
mkdir -p %{buildroot}/usr/share/icons/hicolor/scalable/apps/
# The cargo artefact is named after the crate (nervdesk) since the rename; ship it under the
# name that the service and desktop files packaged here call, /usr/bin/rustdesk. NERV Desk (task-54).
install -m 755 $HBB/target/release/nervdesk %{buildroot}/usr/bin/rustdesk
install $HBB/libsciter-gtk.so %{buildroot}/usr/share/rustdesk/libsciter-gtk.so
install $HBB/res/nervdesk.service %{buildroot}/usr/share/rustdesk/files/
install $HBB/res/128x128@2x.png %{buildroot}/usr/share/icons/hicolor/256x256/apps/nervdesk.png
install $HBB/res/scalable.svg %{buildroot}/usr/share/icons/hicolor/scalable/apps/nervdesk.svg
install $HBB/res/nervdesk.desktop %{buildroot}/usr/share/rustdesk/files/
install $HBB/res/nervdesk-link.desktop %{buildroot}/usr/share/rustdesk/files/

%files
/usr/bin/rustdesk
/usr/share/rustdesk/libsciter-gtk.so
/usr/share/rustdesk/files/nervdesk.service
/usr/share/icons/hicolor/256x256/apps/nervdesk.png
/usr/share/icons/hicolor/scalable/apps/nervdesk.svg
/usr/share/rustdesk/files/nervdesk.desktop
/usr/share/rustdesk/files/nervdesk-link.desktop
/usr/share/rustdesk/files/__pycache__/*

%changelog
# let's skip this for now

%pre
# can do something for centos7
case "$1" in
  1)
    # for install
  ;;
  2)
    # for upgrade
    systemctl stop nervdesk || true
  ;;
esac

%post
systemctl disable rustdesk.service >/dev/null 2>&1 || true
rm -f /etc/systemd/system/rustdesk.service /usr/lib/systemd/system/rustdesk.service /usr/lib/systemd/user/rustdesk.service
cp /usr/share/rustdesk/files/nervdesk.service /etc/systemd/system/nervdesk.service
cp /usr/share/rustdesk/files/nervdesk.desktop /usr/share/applications/
cp /usr/share/rustdesk/files/nervdesk-link.desktop /usr/share/applications/

# NERV Desk (task-76): the desktop entry and icons are installed under the new
# `nervdesk` names now. Remove files left behind by an older `rustdesk` install so
# the application menu cannot list the same product twice.
rm -f /usr/share/applications/rustdesk.desktop /usr/share/applications/rustdesk-link.desktop \
    /usr/share/icons/hicolor/256x256/apps/rustdesk.png \
    /usr/share/icons/hicolor/scalable/apps/rustdesk.svg || true
systemctl daemon-reload
systemctl enable nervdesk
systemctl start nervdesk
update-desktop-database

%preun
case "$1" in
  0)
    # for uninstall
    systemctl stop nervdesk || true
    systemctl disable nervdesk || true
    rm -f /etc/systemd/system/nervdesk.service || true
    rm -f /etc/systemd/system/rustdesk.service /usr/lib/systemd/system/rustdesk.service /usr/lib/systemd/user/rustdesk.service || true
  ;;
  1)
    # for upgrade
  ;;
esac

%postun
case "$1" in
  0)
    # for uninstall
    rm -f /usr/share/applications/nervdesk.desktop /usr/share/applications/nervdesk-link.desktop || true

    # NERV Desk (task-76): the desktop entry and icons are installed under the new
    # `nervdesk` names now. Remove files left behind by an older `rustdesk` install so
    # the application menu cannot list the same product twice.
    rm -f /usr/share/applications/rustdesk.desktop /usr/share/applications/rustdesk-link.desktop \
        /usr/share/icons/hicolor/256x256/apps/rustdesk.png \
        /usr/share/icons/hicolor/scalable/apps/rustdesk.svg || true
    update-desktop-database
  ;;
  1)
    # for upgrade
  ;;
esac
