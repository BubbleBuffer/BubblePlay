// The authenticated session control channel as the upload queue sees it. The channel itself
// lives in the native cores (Rust on Windows, Kotlin on Android); each platform implements
// [CtlBridge] (flutter_rust_bridge, pigeon) and [createPlatformCtlBridge] hands it to the app.

/// The capability both sides must advertise for the log and feedback upload.
const clientLogsCapability = 'client-logs-v1';

/// What happens on the control channel.
sealed class CtlEvent {
  const CtlEvent();
}

/// An authenticated connection to the PC [pcId] is up. [peerCaps] are the capabilities the PC
/// advertised in its hello.
final class CtlOpened extends CtlEvent {
  const CtlOpened(this.pcId, this.peerCaps);

  final String pcId;
  final Set<String> peerCaps;
}

/// A ctl message of an upload kind (`logs-state`, `logs-ack`, `feedback-ack`); the bridge
/// routes no other kinds.
final class CtlMessage extends CtlEvent {
  const CtlMessage(this.json);

  final Map<String, dynamic> json;
}

/// The connection is gone.
final class CtlClosed extends CtlEvent {
  const CtlClosed();
}

abstract interface class CtlBridge {
  /// One [CtlOpened] per authenticated connection, then its messages, then [CtlClosed].
  Stream<CtlEvent> get events;

  /// Sends one ctl message. False when there is no connection or the send queue refused it.
  Future<bool> send(Map<String, dynamic> message);

  /// Bytes waiting in the control channel's send buffer.
  int get bufferedBytes;

  /// Time since the last input event was sent on the session.
  Duration get sinceLastInput;
}
