import 'dart:typed_data';

/// A snapshot of a monitor the host is not streaming (`monitor-previews-v1`).
class MonitorPreview {
  const MonitorPreview({
    required this.monitor,
    required this.width,
    required this.height,
    required this.jpeg,
  });

  /// The `selector` of the monitor it shows.
  final String monitor;
  final int width;
  final int height;
  final Uint8List jpeg;
}

/// Where the session screen gets monitor snapshots. The held pictures live in the native core
/// (Rust on Windows); a core without the feature (Android's Kotlin core) uses
/// [NoMonitorPreviews], and the screen then behaves as it did before the feature.
abstract interface class MonitorPreviewSource {
  /// Removes and returns the held picture of [selector], or null when there is none.
  MonitorPreview? take(String selector);

  /// The media generation of the last live frame shown; it changes with the first live frame
  /// after the host switches monitor. 0 before the first frame.
  int get liveGeneration;
}

/// No snapshots: the feature is absent on this platform.
class NoMonitorPreviews implements MonitorPreviewSource {
  const NoMonitorPreviews();

  @override
  MonitorPreview? take(String selector) => null;

  @override
  int get liveGeneration => 0;
}
