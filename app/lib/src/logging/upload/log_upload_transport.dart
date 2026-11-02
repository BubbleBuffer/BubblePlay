// What the log upload queue needs from the connection to the PC. The real transport (the
// sealed `ctl` channel of the session) is wired by the app later; tests use a fake.

/// The authenticated control channel to one PC, as the uploader sees it.
abstract interface class LogUploadTransport {
  /// Decoded JSON messages from the host (inner `type` messages of the wire doc). The uploader
  /// ignores everything it does not know. A done event means the connection is gone.
  Stream<Map<String, Object?>> get messages;

  /// Sends one JSON message to the host. May throw; the uploader then stops.
  void send(Map<String, Object?> message);

  /// Bytes waiting in the control channel's send buffer.
  int get bufferedBytes;

  /// Time since the last input event was sent on the session.
  Duration get sinceLastInput;
}
