// The Android platform channel: the same snapshot / act / notify contract the Rust core gives
// Windows. Strings only; the JSON is the app contract (see test/fixtures/contract).
// Regenerate with: dart run pigeon --input pigeons/backend.dart
import 'package:pigeon/pigeon.dart';

@ConfigurePigeon(
  PigeonOptions(
    dartOut: 'lib/src/backend/pigeon.g.dart',
    kotlinOut:
        'android/app/src/main/kotlin/de/bubblebuffer/bubbleplay/flutter/Pigeon.g.kt',
    kotlinOptions: KotlinOptions(package: 'de.bubblebuffer.bubbleplay.flutter'),
  ),
)
@HostApi()
abstract class AppHostApi {
  /// The current `AppState` as JSON.
  String snapshot();

  /// Sends one `Action` (JSON). Completes when the core has run it, with an error when it threw;
  /// the effect arrives as a later snapshot.
  @async
  void act(String actionJson);

  /// The stored key as portable `.bpkey` text, for the user to save. Never part of a snapshot.
  String exportKey();
}

@FlutterApi()
abstract class AppFlutterApi {
  /// The snapshot changed.
  void changed();
}

/// The session control channel as the Dart upload queue sees it (docs/architecture/client-log-upload.md):
/// the Kotlin shell hands it the ctl messages of the active session and sends what Dart queues.
class CtlState {
  CtlState({
    required this.open,
    required this.bufferedBytes,
    required this.sinceLastInputMs,
    required this.pcId,
    required this.peerCaps,
  });

  /// An authenticated connection is up.
  bool open;

  /// Bytes waiting in the ctl send buffer.
  int bufferedBytes;

  /// Milliseconds since the last input event was sent; the largest int64 when there was none.
  int sinceLastInputMs;

  /// The connected PC's id; empty when there is no connection.
  String pcId;

  /// The capabilities the PC advertised; empty when there is no connection.
  List<String> peerCaps;
}

@HostApi()
abstract class CtlHostApi {
  /// Sends one ctl message (JSON). False when there is no connection or the queue refused it.
  @async
  bool ctlSend(String json);

  /// The connection's current state (cheap; polled while a connection is open).
  CtlState ctlState();
}

@FlutterApi()
abstract class CtlFlutterApi {
  /// A ctl event: [kind] is `opened` (json: `{"pcId": ..., "peerCaps": [...]}`), `message`
  /// (json: the ctl message) or `closed` (json: `{}`).
  void ctlEvent(String kind, String json);
}
