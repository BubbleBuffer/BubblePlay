import 'dart:io';

import 'ctl_bridge.dart';
import 'ctl_bridge_android.dart';
import 'ctl_bridge_windows.dart';

/// The bridge of this platform, or null when there is none (other platforms, or a platform
/// whose bridge is not written yet). Never throws.
CtlBridge? createPlatformCtlBridge() {
  try {
    if (Platform.isWindows) return createWindowsCtlBridge();
    if (Platform.isAndroid) return createAndroidCtlBridge();
  } catch (_) {}
  return null;
}
