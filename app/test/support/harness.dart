import 'dart:convert';
import 'dart:io';

import 'package:bubbleplay_app/src/app.dart';
import 'package:bubbleplay_app/src/app_controller.dart';
import 'package:bubbleplay_app/src/backend/fake_channel.dart';
import 'package:bubbleplay_app/src/devices/devices_controller.dart';
import 'package:bubbleplay_app/src/feedback/feedback_controller.dart';
import 'package:bubbleplay_app/src/platform/key_files.dart';
import 'package:bubbleplay_app/src/session/stream_channel.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';

/// The generated snapshots of the shared contract, by name (see test/fixtures/contract).
Map<String, Object?> contractStates() {
  final text = File('test/fixtures/contract/states.json').readAsStringSync();
  return (jsonDecode(text) as Map).cast<String, Object?>();
}

/// One named snapshot, optionally changed by [edit] (it gets a deep copy to mutate).
Object? contractState(String name, [void Function(Map<String, Object?> state)? edit]) {
  final copy = jsonDecode(jsonEncode(contractStates()[name])) as Map<String, Object?>;
  edit?.call(copy);
  return copy;
}

/// The two widths every screen is checked at: a desktop window and a phone.
enum Screen {
  desktop(1000, 720),
  phone(390, 780);

  const Screen(this.width, this.height);

  final double width;
  final double height;
}

/// A running app over a fake core.
class TestApp {
  TestApp._(this.channel, this.controller, this.keyFiles);

  final FakeChannel channel;
  final AppController controller;
  final FakeKeyFiles keyFiles;

  /// Decoded actions the app sent, in order.
  List<Object?> get actions => [for (final text in channel.actions) jsonDecode(text)];
}

/// Pumps the whole app at [screen] size, showing [state] (a snapshot, as from [contractState]).
Future<TestApp> pumpApp(
  WidgetTester tester,
  Object? state, {
  Screen screen = Screen.desktop,
  Brightness brightness = Brightness.dark,
  bool windows = true,
  ValueNotifier<Brightness?>? theme,
  StreamChannel? stream,
  DevicesController? devices,
  String? section,
  FeedbackServices? feedback,
}) async {
  tester.view.physicalSize = Size(screen.width, screen.height);
  tester.view.devicePixelRatio = 1;
  addTearDown(tester.view.reset);
  final channel = FakeChannel(state);
  final controller = AppController(channel);
  addTearDown(controller.dispose);
  await controller.refresh();
  final keyFiles = FakeKeyFiles();
  await tester.pumpWidget(
    BubblePlayApp(
      brightness: brightness,
      env: AppEnv(
        controller: controller,
        isWindows: windows,
        keyFiles: keyFiles,
        theme: theme,
        stream: stream ?? const NoStreamChannel(),
        devices: devices,
        initialSection: section,
        feedback: feedback,
      ),
    ),
  );
  await tester.pump();
  return TestApp._(channel, controller, keyFiles);
}

/// A golden scene: a snapshot, plus an optional [setUp] that drives the UI before the picture
/// (open a panel, tap a tab). [windows] forces the platform; by default desktop is Windows and
/// the phone is Android.
class Scene {
  const Scene(this.state, {this.setUp, this.windows});

  final Object? state;
  final Future<void> Function(WidgetTester tester)? setUp;
  final bool? windows;
}

/// Registers one golden per size and theme for each scene of [screen]. `scenes` maps a scene
/// name to a snapshot or a [Scene]. Files land in `goldens/<screen>_<scene>_<size>_<theme>.png`
/// next to the test file and are written on the CI platform (Linux), never on a developer PC.
/// Call from a test file tagged `@Tags(['golden'])`.
void goldenScenes(String screen, Map<String, Object?> scenes) {
  for (final MapEntry(key: sceneName, value: value) in scenes.entries) {
    final scene = value is Scene ? value : Scene(value);
    for (final size in Screen.values) {
      for (final brightness in Brightness.values) {
        final name = '${screen}_${sceneName}_${size.name}_${brightness.name}';
        testWidgets('golden $name', (tester) async {
          await pumpApp(
            tester,
            scene.state,
            screen: size,
            brightness: brightness,
            windows: scene.windows ?? size == Screen.desktop,
          );
          await scene.setUp?.call(tester);
          await expectLater(
            find.byType(WidgetsApp),
            matchesGoldenFile('goldens/$name.png'),
          );
        });
      }
    }
  }
}
