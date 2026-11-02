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
