import 'dart:async';

/// The seam between the Devices UI and the host's devices pipe (docs/app/devices-pipe.md).
///
/// [lines] carries raw JSON lines: every host line verbatim, plus the synthetic
/// `{"type":"connected"}` and `{"type":"disconnected","reason":...}`. Reconnecting is the
/// implementation's job. Lines hold pairing secrets; nobody logs them.
abstract interface class DevicesChannel {
  /// Raw JSON lines from the host.
  Stream<String> get lines;

  /// Sends one command (`{"type":"open"}`, ...) to the host.
  void send(Map<String, Object?> command);

  /// Closes the connection and ends [lines].
  void dispose();
}
