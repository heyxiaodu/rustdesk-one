# The package is named after the OEM identity, not the upstream one. NERV Desk (task-69).
Name:       nervdesk
Version:    1.5.0
Release:    0
Summary:    RPM package
License:    GPL-3.0
URL:        https://rustdesk.com
Vendor:     NERVDesk <info@nervdesk.local>
Requires:   gtk3 libxcb libXfixes alsa-lib libva gstreamer1-plugins-base
Recommends: libayatana-appindicator-gtk3 libxdo
Provides:   libdesktop_drop_plugin.so()(64bit), libdesktop_multi_window_plugin.so()(64bit), libfile_selector_linux_plugin.so()(64bit), libflutter_custom_cursor_plugin.so()(64bit), libflutter_linux_gtk.so()(64bit), libscreen_retriever_plugin.so()(64bit), libtray_manager_plugin.so()(64bit), liburl_launcher_linux_plugin.so()(64bit), libwindow_manager_plugin.so()(64bit), libwindow_size_plugin.so()(64bit), libtexture_rgba_renderer_plugin.so()(64bit)
# See res/rpm.spec for why the Obsoletes/Provides pair is required.
Obsoletes:  rustdesk
Provides:   rustdesk

# https://docs.fedoraproject.org/en-US/packaging-guidelines/Scriptlets/

%description
The best open-source remote desktop client software, written in Rust.

%prep
# we have no source, so nothing here

%build
# we have no source, so nothing here

# %global __python %{__python3}

%install

mkdir -p "%{buildroot}/usr/share/rustdesk" && cp -r ${HBB}/flutter/build/linux/x64/release/bundle/* -t "%{buildroot}/usr/share/rustdesk"
mkdir -p "%{buildroot}/usr/bin"
install -Dm 644 $HBB/res/nervdesk.service -t "%{buildroot}/usr/share/rustdesk/files"
install -Dm 644 $HBB/res/nervdesk.desktop -t "%{buildroot}/usr/share/rustdesk/files"
install -Dm 644 $HBB/res/nervdesk-link.desktop -t "%{buildroot}/usr/share/rustdesk/files"
install -Dm 644 $HBB/res/128x128@2x.png "%{buildroot}/usr/share/icons/hicolor/256x256/apps/nervdesk.png"
install -Dm 644 $HBB/res/scalable.svg "%{buildroot}/usr/share/icons/hicolor/scalable/apps/nervdesk.svg"

%files
/usr/share/rustdesk/*
/usr/share/rustdesk/files/nervdesk.service
/usr/share/icons/hicolor/256x256/apps/nervdesk.png
/usr/share/icons/hicolor/scalable/apps/nervdesk.svg
/usr/share/rustdesk/files/nervdesk.desktop
/usr/share/rustdesk/files/nervdesk-link.desktop

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
# The payload basename is not guaranteed to be "rustdesk": the Flutter bundle is staged under
# that name, but a build that keeps the renamed Sciter artefact ships "nervdesk" instead. Link
# whichever is present -- preferring "rustdesk" -- so that /usr/bin/rustdesk, the path used by
# the .service and .desktop files shipped here, always resolves; fail loudly instead of leaving
# a dangling symlink behind. NERV Desk (task-69).
_linked=0
for _payload in rustdesk nervdesk; do
  if [ -e "/usr/share/rustdesk/$_payload" ]; then
    ln -f -s "/usr/share/rustdesk/$_payload" /usr/bin/rustdesk
    _linked=1
    break
  fi
done
if [ "$_linked" != 1 ]; then
  echo "nervdesk: no payload under /usr/share/rustdesk to link as /usr/bin/rustdesk" >&2
  exit 1
fi
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
    rm /usr/bin/rustdesk || true
    rmdir /usr/lib/rustdesk || true
    rmdir /usr/local/rustdesk || true
    rmdir /usr/share/rustdesk || true
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
    rmdir /usr/lib/rustdesk || true
    rmdir /usr/local/rustdesk || true
  ;;
esac
