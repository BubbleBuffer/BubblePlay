import '../rust/api.dart' as api;
import '../rust/frb_generated.dart';
import 'channel.dart';

/// Windows: the Rust core through flutter_rust_bridge.
class FrbChannel implements AppChannel {
  FrbChannel._();

  /// Loads the native library and opens the key and PC list. Call once, before `runApp`.
  static Future<FrbChannel> open() async {
    await RustLib.init();
    await api.appStart();
    return FrbChannel._();
  }

  @override
  Future<String> snapshot() async => api.appSnapshot();

  @override
  Future<void> act(String actionJson) async => api.appAct(action: actionJson);

  @override
  Future<String> exportKey() => api.appExportKey();

  @override
  Stream<void> get changes => api.appChanges();
}
