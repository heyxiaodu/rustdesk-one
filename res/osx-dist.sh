#!/usr/bin/env bash

echo $MACOS_CODESIGN_IDENTITY
cargo install flutter_rust_bridge_codegen --version 1.80.1 --features uuid --locked
cd flutter; flutter pub get; cd -
~/.cargo/bin/flutter_rust_bridge_codegen --rust-input ./src/flutter_ffi.rs --dart-output ./flutter/lib/generated_bridge.dart --c-output ./flutter/macos/Runner/bridge_generated.h
./build.py --flutter
rm rustdesk-$VERSION.dmg
# PRODUCT_NAME in the xcconfig is the single source of truth for the bundle name; it contains a
# space, so every use below is quoted. Never hardcode the app name in this script again.
APP_NAME="$(sed -n 's/^[[:space:]]*PRODUCT_NAME[[:space:]]*=[[:space:]]*//p' flutter/macos/Runner/Configs/AppInfo.xcconfig | tail -n 1 | sed -e 's/[[:space:]]*$//')"
if [ -z "$APP_NAME" ]; then
  echo "PRODUCT_NAME not found in flutter/macos/Runner/Configs/AppInfo.xcconfig" >&2
  exit 1
fi
APP="flutter/build/macos/Build/Products/Release/$APP_NAME.app"
if [ ! -d "$APP" ]; then
  echo "macOS bundle not found at '$APP'; is PRODUCT_NAME in flutter/macos/Runner/Configs/AppInfo.xcconfig in sync with the Flutter build output?" >&2
  exit 1
fi
# security find-identity -v
codesign --force --options runtime -s $MACOS_CODESIGN_IDENTITY --deep --strict "$APP" -vvv
create-dmg --icon "$APP_NAME.app" 200 190 --hide-extension "$APP_NAME.app" --window-size 800 400 --app-drop-link 600 185 rustdesk-$VERSION.dmg "$APP"
codesign --force --options runtime -s $MACOS_CODESIGN_IDENTITY --deep --strict rustdesk-$VERSION.dmg -vvv
# notarize the rustdesk-${{ env.VERSION }}.dmg
rcodesign notary-submit --api-key-path ~/.p12/api-key.json  --staple rustdesk-$VERSION.dmg
