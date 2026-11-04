import '../rust/api.dart' as api;
import 'monitor_previews.dart';

/// The Windows source: the Rust core's held snapshots (`monitor_preview_take` and
/// `media_generation` in `app/rust/src/api.rs`). `RustLib` must be initialised. Both calls are
/// cheap and synchronous, and never throw into the UI.
class RustMonitorPreviews implements MonitorPreviewSource {
  const RustMonitorPreviews();

  @override
  MonitorPreview? take(String selector) {
    try {
      final data = api.monitorPreviewTake(selector: selector);
      if (data == null) return null;
      return MonitorPreview(
        monitor: data.monitor,
        width: data.width,
        height: data.height,
        jpeg: data.jpeg,
      );
    } catch (_) {
      return null;
    }
  }

  @override
  int get liveGeneration {
    try {
      return api.mediaGeneration();
    } catch (_) {
      return 0;
    }
  }
}
