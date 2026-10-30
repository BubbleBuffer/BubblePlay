import 'dart:async';

/// The seam between the app and its core: the same three calls and one change event on both
/// platforms. Windows implements it over flutter_rust_bridge, Android over Pigeon, tests with a
/// fake. All payloads are the JSON of the shared contract (`AppState` and `Action`).
abstract interface class AppChannel {
  /// The current `AppState` as JSON.
  Future<String> snapshot();

  /// Sends one `Action` as JSON. The result arrives as a later snapshot.
  Future<void> act(String actionJson);

  /// The stored key as portable `.bpkey` text, for the user to save.
  Future<String> exportKey();

  /// One event whenever a new snapshot is ready.
  Stream<void> get changes;
}
