import 'dart:convert';
import 'dart:io';
import 'dart:ui' show Brightness;

import 'package:bubbleplay_app/src/app.dart';
import 'package:bubbleplay_app/src/app_controller.dart';
import 'package:bubbleplay_app/src/backend/fake_channel.dart';
import 'package:bubbleplay_app/src/platform/key_files.dart';
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
      env: AppEnv(controller: controller, isWindows: windows, keyFiles: keyFiles),
    ),
  );
  await tester.pump();
  return TestApp._(channel, controller, keyFiles);
}

/// Registers one golden per size and theme for each scene of [screen]: `scenes` maps a scene
/// name to the snapshot to show. Files land in `test/goldens/<screen>_<scene>_<size>_<theme>.png`
/// and are written on the CI platform (Linux), never on a developer PC. Call from a test file
/// tagged `@Tags(['golden'])`.
void goldenScenes(String screen, Map<String, Object?> scenes) {
  for (final MapEntry(key: scene, value: state) in scenes.entries) {
    for (final size in Screen.values) {
      for (final brightness in Brightness.values) {
        final name = '${screen}_${scene}_${size.name}_${brightness.name}';
        testWidgets('golden $name', (tester) async {
          await pumpApp(
            tester,
            state,
            screen: size,
            brightness: brightness,
            windows: size == Screen.desktop,
          );
          await expectLater(
            find.byType(WidgetsApp),
            matchesGoldenFile('goldens/$name.png'),
          );
        });
      }
    }
  }
}
